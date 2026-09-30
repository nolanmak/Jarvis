#!/usr/bin/env python3
"""Hold the worker's kernel lease across exec into Node."""

import fcntl
import os
import stat
import sys


def main():
    if len(sys.argv) != 3:
        raise SystemExit("usage: lease.py NODE SERVER")
    state = os.environ.get("JARVIS_COMPUTER_STATE", "")
    if not os.path.isabs(state):
        raise SystemExit("JARVIS_COMPUTER_STATE must be an absolute path")
    os.umask(0o077)
    os.makedirs(state, mode=0o700, exist_ok=True)
    state_info = os.stat(state)
    if not stat.S_ISDIR(state_info.st_mode):
        raise SystemExit("worker state is not a directory")
    if state_info.st_uid != os.getuid() or state_info.st_mode & 0o077:
        raise SystemExit("worker state must be owned by this user and mode 0700")
    lock = os.path.join(state, "worker.lock")
    flags = os.O_CREAT | os.O_RDWR
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    fd = os.open(lock, flags, 0o600)
    lock_info = os.fstat(fd)
    if not stat.S_ISREG(lock_info.st_mode):
        raise SystemExit("worker lock is not a regular file")
    if lock_info.st_uid != os.getuid() or lock_info.st_mode & 0o077:
        raise SystemExit("worker lock must be owned by this user and mode 0600")
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        raise SystemExit("computer-use worker already running")
    os.set_inheritable(fd, True)
    os.environ["JARVIS_COMPUTER_LOCK_FD"] = str(fd)
    os.execv(sys.argv[1], [sys.argv[1], sys.argv[2]])


if __name__ == "__main__":
    main()
