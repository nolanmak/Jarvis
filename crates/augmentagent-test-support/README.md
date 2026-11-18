# Executable test fixtures

Use this crate as a **dev-dependency** when a Rust test creates a fake CLI,
converter or Git hook. `write_executable` publishes complete bytes with owner
execute permission (and preserves the mode when replacing an existing file).
`copy_executable` preserves the source bytes and permissions. Existing callers
can still explicitly set a particular final mode.

The test process writes only a private, non-executable input file. A short-lived
`/bin/cp` child creates the executable inode, and the helper waits for that child
before chmod and atomic rename. Other Rust test threads cannot inherit a write
fd for the executable inode. No exec retry is used.

Closing a parent-owned writer before spawning is insufficient: another thread
may already have forked and retained it. CLOEXEC closes that inherited fd only
when the other child execs. Renaming a parent-written tempfile also preserves
its inode and inherited writers. Both can therefore still produce Linux
ETXTBSY. Keeping the executable writer out of the multithreaded test process
avoids that race.

The Linux tests hold a real fork before exec at publication, execute fixtures
from eight concurrent threads, and verify atomic replacement with an inherited
old writer still open. The existing agents CI step runs this crate's tests.
