//! Unit-test global allocator that counts the bytes each thread requests and frees, so a test
//! can compare a live reservation with the storage it stands for. Counts layout sizes (what Rust
//! requests), per thread, so concurrent tests do not disturb each other.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct Counting;

thread_local! {
    static NET: Cell<i64> = const { Cell::new(0) };
    static PEAK: Cell<i64> = const { Cell::new(0) };
}

fn add(bytes: i64) {
    // A thread being torn down may still allocate; it is simply not counted.
    let _ = NET.try_with(|net| {
        let value = net.get() + bytes;
        net.set(value);
        let _ = PEAK.try_with(|peak| peak.set(peak.get().max(value)));
    });
}

// SAFETY: every call forwards to `System` with the caller's arguments unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        add(layout.size() as i64);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        add(-(layout.size() as i64));
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        add(layout.size() as i64);
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        add(new_size as i64 - layout.size() as i64);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

fn net() -> i64 {
    NET.with(Cell::get)
}

/// Runs `f` and returns its result with the bytes this thread allocated in `f` and still
/// holds afterwards: what the result retains, plus anything `f` leaked elsewhere.
pub(crate) fn measure<R>(f: impl FnOnce() -> R) -> (R, u64) {
    let (result, retained, _) = measure_peak(f);
    (result, retained)
}

/// `measure`, also returning the most bytes this thread held at once during `f`, above what
/// it held before.
pub(crate) fn measure_peak<R>(f: impl FnOnce() -> R) -> (R, u64, u64) {
    let before = net();
    let outer = PEAK.with(Cell::get);
    PEAK.with(|peak| peak.set(before));
    let result = f();
    let after = net();
    let peak = PEAK.with(Cell::get);
    PEAK.with(|cell| cell.set(outer.max(peak)));
    (
        result,
        u64::try_from(after - before).unwrap_or(0),
        u64::try_from(peak - before).unwrap_or(0),
    )
}
