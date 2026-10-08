#!/usr/bin/python3
"""Trusted VM supervisor. Never import task packages in this root process.

Launched with -I -S. User code runs in a separate unprivileged interpreter.
Only this supervisor can write the terminal serial result. The workload's
stdout/stderr are bounded pipes, not the QEMU serial device.
"""
import base64
import ctypes
import hashlib
import json
import os
from pathlib import Path
import resource
import selectors
import signal
import stat
import subprocess

MAX_CAPTURE = 8 * 1024 * 1024
MAX_FILE = 32 * 1024 * 1024
MAX_EXPORT = 64 * 1024 * 1024


def reap_workload(uid):
    # All processes with this uid belong to this single-use guest workload.
    # Kill detached children too, before examining output files.
    for entry in Path('/proc').iterdir():
        if entry.name.isdigit():
            try:
                if entry.stat().st_uid == uid:
                    os.kill(int(entry.name), signal.SIGKILL)
            except (OSError, ProcessLookupError):
                pass
    while True:
        try:
            os.waitpid(-1, 0)
        except ChildProcessError:
            break


def export_files(names):
    result = {}
    total = 0
    directory = os.open('/outputs', os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        for name in names:
            descriptor = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
            with os.fdopen(descriptor, 'rb') as stream:
                info = os.fstat(stream.fileno())
                if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
                    raise ValueError('output_denied')
                if info.st_size > MAX_FILE or total + info.st_size > MAX_EXPORT:
                    raise ValueError('resource_limit')
                data = stream.read(MAX_FILE + 1)
                after = os.fstat(stream.fileno())
                if (after.st_size != info.st_size or after.st_mtime_ns != info.st_mtime_ns
                        or len(data) != info.st_size):
                    raise ValueError('output_denied')
                total += len(data)
                result[name] = {'data': base64.b64encode(data).decode(), 'bytes': len(data),
                                'sha256': hashlib.sha256(data).hexdigest()}
    finally:
        os.close(directory)
    return result


def main():
    job = json.loads(Path('/job.json').read_text())
    uid, gid = job['uid'], job['gid']
    libc = ctypes.CDLL(None)
    if libc.prctl(36, 1, 0, 0, 0) != 0:  # subreaper, including detached children
        raise RuntimeError('cannot own workload descendants')
    for name in ('/work', '/outputs', '/home/worker'):
        os.chown(name, uid, gid)
    Path('/etc/passwd').write_text(f'root:x:0:0:root:/root:/bin/sh\nworker:x:{uid}:{gid}:worker:/home/worker:/bin/sh\n')
    Path('/etc/group').write_text(f'root:x:0:\nworker:x:{gid}:\n')
    environment = {'PATH': '/usr/bin:/bin', 'HOME': '/home/worker', 'TMPDIR': '/tmp',
                   'LANG': 'C.UTF-8', 'PYTHONDONTWRITEBYTECODE': '1'}

    def worker():
        if libc.prctl(38, 1, 0, 0, 0) != 0:
            raise OSError('cannot enforce no-new-privileges')
        os.setgroups([])
        os.setgid(gid)
        os.setuid(uid)
        os.umask(0o077)
        resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
        resource.setrlimit(resource.RLIMIT_NPROC, (128, 128))
        resource.setrlimit(resource.RLIMIT_NOFILE, (256, 256))
        resource.setrlimit(resource.RLIMIT_FSIZE, (MAX_FILE, MAX_FILE))
        memory = job['memory_mb'] * 1024 * 1024
        resource.setrlimit(resource.RLIMIT_AS, (memory, memory))

    process = subprocess.Popen(['/usr/bin/python3', '-I', '/program.py'], cwd='/work', env=environment,
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        start_new_session=True, preexec_fn=worker)
    logs = {'stdout': bytearray(), 'stderr': bytearray()}
    total = 0
    error = None
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ, 'stdout')
            selector.register(process.stderr, selectors.EVENT_READ, 'stderr')
            while selector.get_map() and error is None:
                for key, _ in selector.select(0.1):
                    data = os.read(key.fileobj.fileno(), 65536)
                    if not data:
                        selector.unregister(key.fileobj)
                        key.fileobj.close()
                        continue
                    total += len(data)
                    if total > MAX_CAPTURE:
                        error = 'resource_limit'
                        break
                    logs[key.data].extend(data)
        if error:
            process.kill()
        status = process.wait()
    finally:
        reap_workload(uid)
    files = {}
    if error is None and status:
        error = 'resource_limit' if status in (-signal.SIGKILL, -signal.SIGXFSZ) else 'execution_failed'
    if error is None:
        try:
            files = export_files(job['outputs'])
        except ValueError as exc:
            error = str(exc)
        except OSError:
            error = 'output_denied'
    result = {'ok': error is None, 'exitCode': status, 'error': error,
              'stdout': logs['stdout'].decode(errors='replace'),
              'stderr': logs['stderr'].decode(errors='replace'), 'files': files if error is None else {}}
    print('\nJARVIS_COMPUTE_RESULT:' + json.dumps(result, separators=(',', ':')), flush=True)


if __name__ == '__main__':
    main()
