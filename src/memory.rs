//! Return temporary catalog/response allocations after the client becomes idle.
/// glibc can retain tens of MiB of freed JSON nodes between live small objects.
/// This releases only allocator-owned free pages; live values and caches remain.
pub(crate) fn release_unused_heap() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim is a thread-safe allocator operation. It does not
    // invalidate any live allocation or change ownership of Rust values.
    unsafe {
        libc::malloc_trim(0);
    }
}
