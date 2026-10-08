#!/usr/bin/python3
"""Trusted VM supervisor. Never import task packages in this root process.

Launched with -I -S. User code runs in a separate unprivileged interpreter.
Only this supervisor can write the virtio result channel. The workload's
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
import sys
import time

MAX_CAPTURE = 8 * 1024 * 1024
MAX_FILE = 32 * 1024 * 1024
MAX_EXPORT = 64 * 1024 * 1024


# Runs after dropping privileges and setting resource limits. The exit code is
# only an error classification, never an authorization or cleanup signal. User
# code can choose this exit code just as it can kill itself with SIGXFSZ.
RESOURCE_EXIT = 125
WORKER_BOOTSTRAP = """import errno,os,runpy,sys,traceback
program = sys.argv[1]
sys.argv = [program]
try:
    runpy.run_path(program, run_name='__main__')
except (MemoryError, OSError) as error:
    if isinstance(error, MemoryError) or error.errno in (errno.ENOSPC, errno.EDQUOT, errno.EFBIG, errno.ENOMEM, errno.EAGAIN):
        try:
            traceback.print_exc()
            sys.stdout.flush()
            sys.stderr.flush()
        finally:
            os._exit(125)
    raise
"""


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


def seal_environment(root):
    """Make prior environments inaccessible to later preparation workers."""
    for base, directories, files in os.walk(root, followlinks=False):
        for name in [*directories, *files]:
            path = Path(base) / name
            info = path.lstat()
            if stat.S_ISLNK(info.st_mode) or not (stat.S_ISDIR(info.st_mode) or stat.S_ISREG(info.st_mode)):
                raise ValueError('invalid installed file')
            if stat.S_ISREG(info.st_mode) and info.st_nlink != 1:
                raise ValueError('linked installed file')
            os.chown(path, 0, 0, follow_symlinks=False)
            path.chmod(0o555 if stat.S_ISDIR(info.st_mode) or info.st_mode & 0o111 else 0o444)
    os.chown(root, 0, 0); root.chmod(0o555)
    descriptor = os.open(root / 'lock.json', os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor) as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_size > 1024 * 1024 or info.st_nlink != 1:
            raise ValueError('invalid dependency lock')
        value = json.load(stream)
    if not isinstance(value, list) or not 1 <= len(value) <= 256:
        raise ValueError('invalid resolved package count')
    return value


def result_channel():
    # devtmpfs creates the port for the trusted supervisor. Make ownership and
    # access explicit before starting any workload. Popen closes this fd in the
    # unprivileged interpreter, including during package preparation.
    for port in Path('/sys/class/virtio-ports').iterdir():
        if (port / 'name').read_text().strip() == 'org.jarvis.compute.result':
            descriptor = os.open('/dev/' + port.name, os.O_WRONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
            try:
                info = os.fstat(descriptor)
                if not stat.S_ISCHR(info.st_mode) or info.st_uid != 0:
                    raise RuntimeError('untrusted result channel')
                os.fchmod(descriptor, 0o600)
                return os.fdopen(descriptor, 'wb')
            except BaseException:
                os.close(descriptor)
                raise
    raise RuntimeError('runtime requires the virtio console driver')


def main():
    channel = result_channel()
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

    registry = None
    program = '/program.py'
    if job['prepare']:
        cache = Path('/cache')
        os.chown(cache, 0, 0); cache.chmod(0o755)
        envs = cache / 'envs'; envs.mkdir(exist_ok=True); envs.chmod(0o755)
        target = envs / job['environmentId']
        target.mkdir(mode=0o700)
        os.chown(target, uid, gid)
        registry = make_proxy('/root/control', python=True)
        program = '/prepare.py'
    elif job.get('environmentId'):
        site_path = '/cache/envs/' + job['environmentId'] + '/site'
        Path('/execute.py').write_text('import site,runpy\nsite.addsitedir(' + repr(site_path) + ')\nrunpy.run_path("/program.py",run_name="__main__")\n')
        os.chmod('/execute.py', 0o444)
        program = '/execute.py'
    Path('/worker.py').write_text(WORKER_BOOTSTRAP)
    os.chmod('/worker.py', 0o444)
    process = subprocess.Popen(['/usr/bin/python3', '-I', '-B', '/worker.py', program], cwd='/work', env=environment,
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        start_new_session=True, preexec_fn=worker)
    if registry is not None:
        threading.Thread(target=registry.serve_forever, daemon=True).start()
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
        error = 'resource_limit' if status in (RESOURCE_EXIT, -signal.SIGKILL, -signal.SIGXFSZ) else 'execution_failed'
    locked = []
    if job['prepare'] and error is not None:
        error = ('dependency_policy_denied' if b'JARVIS_DEPENDENCY_POLICY' in logs['stderr']
                 else 'resource_limit' if error == 'resource_limit' else 'dependency_unavailable')
    if job['prepare'] and error is None:
        try:
            locked = seal_environment(Path('/cache/envs') / job['environmentId'])
        except (OSError, ValueError):
            error = 'dependency_integrity'
    if error is None:
        try:
            files = export_files(job['outputs'])
        except ValueError as exc:
            error = str(exc)
        except OSError:
            error = 'output_denied'
    result = {'ok': error is None, 'exitCode': status, 'error': error,
              'privateLogs': {name: base64.b64encode(data).decode() for name, data in logs.items()},
              'files': files if error is None else {}, 'dependencyLock': locked}
    with channel:
        channel.write(('\nJARVIS_COMPUTE_RESULT:' + json.dumps(result, separators=(',', ':')) + '\n').encode())


if __name__ == '__main__':
    main()
