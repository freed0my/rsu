//! Bounded wait-free single-producer single-consumer queue for `Copy`
//! messages. Capacity is rounded up to a power of two.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[repr(align(64))]
struct Padded(AtomicUsize);

struct Shared<T> {
    head: Padded, // next slot to read, owned by the consumer
    tail: Padded, // next slot to write, owned by the producer
    mask: usize,
    buf: Box<[UnsafeCell<MaybeUninit<T>>]>,
}

// SAFETY: each slot is written by the single producer before `tail` is
// published and read by the single consumer only after that.
unsafe impl<T: Send> Sync for Shared<T> {}
unsafe impl<T: Send> Send for Shared<T> {}

pub struct Producer<T> {
    s: Arc<Shared<T>>,
}

pub struct Consumer<T> {
    s: Arc<Shared<T>>,
}

pub fn channel<T: Copy + Send>(capacity: usize) -> (Producer<T>, Consumer<T>) {
    let cap = capacity.max(2).next_power_of_two();
    let buf = (0..cap).map(|_| UnsafeCell::new(MaybeUninit::uninit())).collect::<Vec<_>>().into_boxed_slice();
    let s =
        Arc::new(Shared { head: Padded(AtomicUsize::new(0)), tail: Padded(AtomicUsize::new(0)), mask: cap - 1, buf });
    (Producer { s: s.clone() }, Consumer { s })
}

impl<T: Copy + Send> Producer<T> {
    /// Returns the value back if the queue is full. Never blocks.
    #[inline]
    pub fn push(&mut self, v: T) -> Result<(), T> {
        let s = &*self.s;
        let tail = s.tail.0.load(Ordering::Relaxed);
        let head = s.head.0.load(Ordering::Acquire);
        if tail.wrapping_sub(head) > s.mask {
            return Err(v);
        }
        // SAFETY: the slot at `tail` is outside the readable range [head, tail).
        unsafe { (*s.buf[tail & s.mask].get()).write(v) };
        s.tail.0.store(tail.wrapping_add(1), Ordering::Release);
        Ok(())
    }
}

impl<T: Copy + Send> Consumer<T> {
    #[inline]
    pub fn pop(&mut self) -> Option<T> {
        let s = &*self.s;
        let head = s.head.0.load(Ordering::Relaxed);
        let tail = s.tail.0.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        // SAFETY: head < tail, so the producer has initialised this slot.
        let v = unsafe { (*s.buf[head & s.mask].get()).assume_init_read() };
        s.head.0.store(head.wrapping_add(1), Ordering::Release);
        Some(v)
    }

    pub fn len(&self) -> usize {
        let s = &*self.s;
        s.tail.0.load(Ordering::Acquire).wrapping_sub(s.head.0.load(Ordering::Relaxed))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_and_drains_in_order() {
        let (mut tx, mut rx) = channel::<u32>(4);
        for i in 0..4 {
            tx.push(i).unwrap();
        }
        assert_eq!(tx.push(99), Err(99));
        for i in 0..4 {
            assert_eq!(rx.pop(), Some(i));
        }
        assert_eq!(rx.pop(), None);
    }

    #[test]
    fn nothing_is_lost_across_threads() {
        const N: u64 = 2_000_000;
        let (mut tx, mut rx) = channel::<u64>(1024);
        let t = std::thread::spawn(move || {
            let mut i = 0;
            while i < N {
                if tx.push(i).is_ok() {
                    i += 1;
                } else {
                    std::hint::spin_loop();
                }
            }
        });
        let mut expect = 0;
        while expect < N {
            if let Some(v) = rx.pop() {
                assert_eq!(v, expect);
                expect += 1;
            }
        }
        t.join().unwrap();
    }
}
