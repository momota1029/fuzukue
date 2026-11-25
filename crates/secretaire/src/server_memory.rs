//! Bound glibc arena growth and give idle allocations back to Linux.

#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub(crate) fn configure_allocator() {
    // Keep administrator overrides from the environment, including GLIBC_TUNABLES.
    if std::env::var_os("MALLOC_ARENA_MAX").is_none()
        && !std::env::var("GLIBC_TUNABLES").unwrap_or_default().split(':')
            .any(|setting| setting.starts_with("glibc.malloc.arena_max="))
    {
        unsafe { libc::mallopt(libc::M_ARENA_MAX, 4); }
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub(crate) fn start_trimming() {
    use std::time::Duration;
    tokio::spawn(async {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let _ = tokio::task::spawn_blocking(|| unsafe { libc::malloc_trim(0) }).await;
        }
    });
}
