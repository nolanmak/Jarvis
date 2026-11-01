#!/usr/bin/env python3
"""Validate both Cargo artifacts before removing group/other inode write access."""
import contextlib
import os
import pathlib
import stat
import sys

def secure_release_executables(directory):
 # Cargo links its public executable to the deps artifact. Keep those links
 # and bytes intact; remove only group/other write access on the opened inode.
 paths=[pathlib.Path(directory)/name for name in ('augmentagent','augmentagent-mcp-memory')]
 if not any(path.exists() or path.is_symlink() for path in paths):return
 with contextlib.ExitStack() as opened:
  files=[]
  for path in paths:
   stream=opened.enter_context(os.fdopen(os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK),'rb'))
   info=os.fstat(stream.fileno())
   if not stat.S_ISREG(info.st_mode) or info.st_uid not in (0,os.geteuid()) or not info.st_mode & 0o111 or not 0<info.st_size<=1024**3:
    raise RuntimeError('unsafe release executable')
   current=path.stat(follow_symlinks=False)
   if (current.st_dev,current.st_ino)!=(info.st_dev,info.st_ino):raise RuntimeError('release executable changed')
   files.append((stream,info))
  for stream,info in files:
   mode=stat.S_IMODE(info.st_mode)
   if mode & 0o022:
    os.fchmod(stream.fileno(),mode & ~0o022);os.fsync(stream.fileno())
if __name__ == '__main__':
 secure_release_executables(sys.argv[1])
