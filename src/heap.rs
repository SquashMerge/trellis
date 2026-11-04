//! Process heap statistics for the opt-in S phase markers (#669 H8): how much of the
//! daemon's resident heap is in use and how much the allocator holds free.

/// Bytes the heap has in use and holds from the system (macOS malloc zones; zeros
/// elsewhere), for opt-in diagnostics only.
pub(crate) fn bytes() -> (u64, u64) {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: a null zone asks for the statistics of every zone; `stats` is written fully.
        let mut stats: libc::malloc_statistics_t = unsafe { std::mem::zeroed() };
        unsafe { libc::malloc_zone_statistics(std::ptr::null_mut(), &mut stats) };
        (stats.size_in_use as u64, stats.size_allocated as u64)
    }
    #[cfg(not(target_os = "macos"))]
    {
        (0, 0)
    }
}
