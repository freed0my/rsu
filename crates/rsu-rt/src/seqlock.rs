//! "Latest value" cell: one writer that never waits, any number of readers
//! that retry if they raced a write. This is what lets the render thread
//! pick up the newest cursor sample without going through a queue.

use std::cell::UnsafeCell;
use std::sync::atomic::{fence, AtomicU64, Ordering};
use std::sync::Arc;

struct Shared<T> {
    seq: AtomicU64,
    val: UnsafeCell<T>,
}

// SAFETY: readers only ever copy `val` out and discard the copy unless the
// sequence number proves no write overlapped the read.
unsafe impl<T: Copy + Send> Sync for Shared<T> {}
unsafe impl<T: Copy + Send> Send for Shared<T> {}

/// Not `Clone`: owning it is what makes the holder the single writer.
pub struct Writer<T> {
    s: Arc<Shared<T>>,
}

#[derive(Clone)]
pub struct Reader<T> {
    s: Arc<Shared<T>>,
}

pub fn channel<T: Copy + Send>(init: T) -> (Writer<T>, Reader<T>) {
    let s = Arc::new(Shared { seq: AtomicU64::new(0), val: UnsafeCell::new(init) });
    (Writer { s: s.clone() }, Reader { s })
}

impl<T: Copy + Send> Writer<T> {
    #[inline]
    pub fn write(&mut self, v: T) {
        let s = &*self.s;
        let seq = s.seq.load(Ordering::Relaxed);
        s.seq.store(seq.wrapping_add(1), Ordering::Relaxed); // odd: write in progress
        fence(Ordering::Release);
        // SAFETY: `&mut self` on the only Writer means no concurrent writer.
        unsafe { std::ptr::write_volatile(s.val.get(), v) };
        s.seq.store(seq.wrapping_add(2), Ordering::Release);
    }
}

impl<T: Copy + Send> Reader<T> {
    #[inline]
    pub fn read(&self) -> T {
        let s = &*self.s;
        loop {
            let a = s.seq.load(Ordering::Acquire);
            if a & 1 == 0 {
                // SAFETY: the copy may be torn if a write overlaps; it is only
                // returned when the sequence number is unchanged, which rules
                // that out. T: Copy, so a torn copy is never dropped.
                let v = unsafe { std::ptr::read_volatile(s.val.get()) };
                fence(Ordering::Acquire);
                if s.seq.load(Ordering::Relaxed) == a {
                    return v;
                }
            }
            std::hint::spin_loop();
        }
    }

    /// Number of completed writes, for "did anything change" checks.
    #[inline]
    pub fn version(&self) -> u64 {
        self.s.seq.load(Ordering::Acquire) >> 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readers_never_see_a_torn_value() {
        let (mut w, r) = channel((0u64, 0u64, 0u64));
        let t = std::thread::spawn(move || {
            for i in 1..=1_000_000u64 {
                w.write((i, i.wrapping_mul(3), !i));
            }
        });
        let mut last = 0;
        while last < 1_000_000 {
            let (a, b, c) = r.read();
            assert_eq!(b, a.wrapping_mul(3));
            if a != 0 {
                assert_eq!(c, !a);
            }
            assert!(a >= last);
            last = a;
        }
        t.join().unwrap();
    }
}
