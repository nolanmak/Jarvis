//! Linux child-process descriptor hygiene for Code Mode.

/// Mark every descriptor except the three deliberately configured standard
/// streams close-on-exec. CLOEXEC preserves Rust's spawn-error pipe until exec
/// succeeds (unlike closing it here), and avoids iterating a racing fd table.
/// Failure must prevent spawn rather than permit ambient descriptor authority.
///
/// # Safety
/// Call only in the forked child's pre_exec hook. Changing the parent's fd flags
/// would interfere with other tasks. This uses only a syscall and errno access.
pub(super) unsafe fn isolate_descriptors() -> std::io::Result<()> {
    let result = unsafe {
        libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, libc::CLOSE_RANGE_CLOEXEC)
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
