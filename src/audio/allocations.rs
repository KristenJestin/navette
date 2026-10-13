//! The tests' allocator: it counts the allocations of each thread, so a test can check that a
//! realtime path does not allocate.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct Counting;

thread_local! {
    static COUNT: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: every call goes to the system allocator unchanged; only a thread-local counter is
// added, which does not allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        COUNT.with(|count| count.set(count.get() + 1));
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        COUNT.with(|count| count.set(count.get() + 1));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// How many times `f` allocated, on this thread.
pub fn during(f: impl FnOnce()) -> usize {
    let before = COUNT.with(Cell::get);
    f();
    COUNT.with(Cell::get) - before
}
