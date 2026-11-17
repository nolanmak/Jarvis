//! Utilities for executable fixtures; depend on this crate only from tests.
#![cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{io, path::Path};

/// Publish a complete owner-executable fixture without retries.
pub fn write_executable(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> io::Result<()> {
    publish(path.as_ref(), bytes.as_ref(), || {})
}

/// Copy an executable fixture without opening the destination for writing in
/// this process. Source bytes and permissions are preserved.
pub fn copy_executable(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let bytes = std::fs::read(from.as_ref())?;
    let permissions = std::fs::metadata(from.as_ref())?.permissions();
    write_executable(to.as_ref(), bytes)?;
    std::fs::set_permissions(to, permissions)
}

fn publish(path: &Path, bytes: &[u8], before_publish: impl FnOnce()) -> io::Result<()> {
    let mode = match std::fs::metadata(path) {
        Ok(metadata) => metadata.permissions().mode() & 0o777,
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0o700,
        Err(error) => return Err(error),
    };
    let directory = tempfile::tempdir_in(path.parent().unwrap_or_else(|| Path::new(".")))?;
    let staged = directory.path().join("script");
    // Only this non-executable input inode is opened for writing in the
    // test process. A fork may retain it, but it is never the exec target.
    let input = directory.path().join("contents");
    std::fs::write(&input, bytes)?;
    // The child alone opens the executable inode for writing. Waiting for
    // it also avoids an EOF dependency on pipe fds inherited by other forks.
    let status = std::process::Command::new("/bin/cp")
        .arg(&input)
        .arg(&staged)
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!("fixture writer failed: {status}")));
    }
    before_publish();
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(staged, path)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::process::Command;

    // A deliberately paused fork represents another test thread's child
    // between fork and exec. The child uses only async-signal-safe syscalls.
    struct ForkHold {
        release: OwnedFd,
        pid: libc::pid_t,
    }
    impl ForkHold {
        fn new() -> Self {
            let mut release = [0; 2];
            let mut ready = [0; 2];
            unsafe {
                assert_eq!(libc::pipe2(release.as_mut_ptr(), libc::O_CLOEXEC), 0);
                assert_eq!(libc::pipe2(ready.as_mut_ptr(), libc::O_CLOEXEC), 0);
                let pid = libc::fork();
                assert!(pid >= 0);
                if pid == 0 {
                    libc::close(release[1]);
                    libc::close(ready[0]);
                    let byte = 1u8;
                    libc::write(ready[1], (&byte as *const u8).cast(), 1);
                    let mut byte = 0u8;
                    libc::read(release[0], (&mut byte as *mut u8).cast(), 1);
                    libc::_exit(0);
                }
                libc::close(release[0]);
                libc::close(ready[1]);
                let mut byte = 0u8;
                assert_eq!(libc::read(ready[0], (&mut byte as *mut u8).cast(), 1), 1);
                libc::close(ready[0]);
                Self {
                    release: OwnedFd::from_raw_fd(release[1]),
                    pid,
                }
            }
        }
    }
    impl Drop for ForkHold {
        fn drop(&mut self) {
            unsafe {
                let byte = 1u8;
                libc::write(self.release.as_raw_fd(), (&byte as *const u8).cast(), 1);
                libc::waitpid(self.pid, std::ptr::null_mut(), 0);
            }
        }
    }

    #[test]
    fn published_script_executes_while_a_concurrent_fork_is_held_before_exec() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fixture");
        let mut child = None;
        publish(&path, b"#!/bin/sh\nexit 0\n", || {
            child = Some(ForkHold::new())
        })
        .unwrap();
        let outcome = Command::new(&path).status();
        drop(child);
        assert!(
            outcome.as_ref().is_ok_and(|s| s.success()),
            "published executable: {outcome:?}"
        );
    }
    #[test]
    fn fixture_scripts_execute_in_parallel_while_forks_hold_inherited_descriptors() {
        let root = tempfile::tempdir().unwrap();
        let workers: Vec<_> = (0..8)
            .map(|worker| {
                let root = root.path().to_owned();
                std::thread::spawn(move || {
                    for round in 0..25 {
                        let path = root.join(format!("script-{worker}-{round}"));
                        let mut child = None;
                        publish(&path, b"#!/bin/sh\nexit 0\n", || {
                            child = Some(ForkHold::new())
                        })
                        .unwrap();
                        let status = Command::new(&path).status();
                        drop(child);
                        assert!(
                            status.as_ref().is_ok_and(|s| s.success()),
                            "{path:?}: {status:?}"
                        );
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
    }

    #[test]
    fn replacement_does_not_reuse_an_open_writer_inode_and_preserves_mode() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("with spaces and 'quotes'");
        write_executable(&path, b"#!/bin/sh\nprintf old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let old = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let held = ForkHold::new();
        drop(old);
        let bytes = b"#!/bin/sh\nprintf new\n";
        write_executable(&path, bytes).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let output = Command::new(&path).output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"new");
        drop(held);
    }
    #[test]
    fn executable_copy_preserves_bytes_and_permissions() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let target = directory.path().join("target");
        write_executable(&source, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
        copy_executable(&source, &target).unwrap();
        assert_eq!(
            std::fs::read(source).unwrap(),
            std::fs::read(&target).unwrap()
        );
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(Command::new(target).status().unwrap().success());
    }
}
