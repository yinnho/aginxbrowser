//! Hand freed heap back to the OS once a session's thread is gone.
//!
//! Dropping a session frees tens of MB, but the system allocator keeps the
//! pages in its own caches (macOS: "Malloc Large (empty)" plus fragmented
//! small zones), so RSS stays at the page's high-water mark until restart.

#[cfg(target_os = "macos")]
extern "C" {
    fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
}

pub fn release_to_os() {
    // SAFETY: both calls only ask the allocator to return free pages; they
    // take no pointers to live data (a null zone means "all zones").
    #[cfg(target_os = "macos")]
    unsafe {
        malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::malloc_trim(0);
    }
}
