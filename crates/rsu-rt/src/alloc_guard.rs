//! Global allocator wrapper that counts heap allocations made by a thread
//! while it is inside a "hot" section (the frame). The count is shown in the
//! overlay; the goal for gameplay frames is a flat zero.
//!
//! This only sees Rust-side allocations. The Vulkan driver, libwayland and
//! libpipewire call malloc directly and are invisible here.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

thread_local! {
    static HOT: Cell<bool> = const { Cell::new(false) };
}

static HOT_ALLOCS: AtomicU64 = AtomicU64::new(0);

pub struct CountingAlloc;

#[inline]
fn note() {
    if HOT.try_with(|h| h.get()).unwrap_or(false) {
        HOT_ALLOCS.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: defers to System for all memory management; only adds counting.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        System.alloc(layout)
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        System.alloc_zeroed(layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        System.realloc(ptr, layout, new_size)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
}

/// Marks the current thread as inside the hot section until dropped.
pub struct HotSection(());

impl HotSection {
    pub fn enter() -> Self {
        HOT.with(|h| h.set(true));
        HotSection(())
    }
}

impl Drop for HotSection {
    fn drop(&mut self) {
        HOT.with(|h| h.set(false));
    }
}

pub fn hot_allocs() -> u64 {
    HOT_ALLOCS.load(Ordering::Relaxed)
}
