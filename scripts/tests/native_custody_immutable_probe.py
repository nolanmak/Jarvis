"""Probe immutable-file behavior on disposable files only; no runtime grant.

Needs CAP_LINUX_IMMUTABLE (normally sudo). It accepts no paths or options,
creates its own temporary directory, and clears only flags it sets there.
It neither reads nor changes the native store, policy, services or budgets.
"""
import array
import errno
import fcntl
import json
import mmap
import os
from pathlib import Path
import resource
import signal
import sys
import tempfile
import time

GETFLAGS = 0x80086601
SETFLAGS = 0x40086602
IMMUTABLE = 0x10


def flags(fd):
    value = array.array('l', [0])
    fcntl.ioctl(fd, GETFLAGS, value, True)
    return value[0]


def change(fd, value):
    fcntl.ioctl(fd, SETFLAGS, array.array('l', [value]))


def error_result(operation):
    try:
        operation()
        return {'denied': False}
    except OSError as error:
        return {'denied': error.errno in (errno.EPERM, errno.EACCES),
                'errno': error.errno}


def wait_child(pid):
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        ended, status = os.waitpid(pid, os.WNOHANG)
        if ended:
            return status
        time.sleep(0.005)
    raise TimeoutError('disposable writer did not finish')


def run_case(kind):
    with tempfile.TemporaryDirectory(prefix='native-custody-immutable-probe-') as temp:
        path = Path(temp) / 'fixture'
        if kind == 'directory':
            path.mkdir(mode=0o700)
            (path / 'original').write_bytes(b'original')
        else:
            path.write_bytes(b'original')
        readonly = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
        original_flags = flags(readonly)
        writable, mapping, child = None, None, None
        lease = False
        interrupted = []
        previous = signal.signal(signal.SIGIO, lambda *_: interrupted.append(True))
        def deadline_expired(*_):
            raise TimeoutError('disposable immutable probe timed out')
        previous_alarm = signal.signal(signal.SIGALRM, deadline_expired)
        signal.alarm(10)
        result = {'case': kind, 'fixture_only': True}
        try:
            if kind == 'existing-fd':
                writable = os.open(path, os.O_WRONLY)
            elif kind == 'existing-mapping':
                descriptor = os.open(path, os.O_RDWR)
                mapping = mmap.mmap(descriptor, 0, access=mmap.ACCESS_WRITE)
                os.close(descriptor)
            elif kind == 'pending-open':
                fcntl.fcntl(readonly, fcntl.F_SETLEASE, fcntl.F_RDLCK)
                lease = True
                child = os.fork()
                if child == 0:
                    os.close(readonly)
                    try:
                        descriptor = os.open(path, os.O_WRONLY)
                        os.write(descriptor, b'changed!')
                        os.close(descriptor)
                        os._exit(0)
                    except OSError as error:
                        os._exit(80 if error.errno in (errno.EPERM, errno.EACCES) else 81)
                deadline = time.monotonic() + 5
                while not interrupted and time.monotonic() < deadline:
                    time.sleep(0.005)
                if not interrupted:
                    raise TimeoutError('disposable writer did not reach lease break')
            try:
                change(readonly, original_flags | IMMUTABLE)
            except OSError as error:
                result.update(flag_enabled=False, errno=error.errno,
                              supported=False)
                return result
            result['flag_enabled'] = bool(flags(readonly) & IMMUTABLE)
            if kind == 'directory':
                def create_new():
                    descriptor = os.open(path / 'new', os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
                    os.close(descriptor)
                creation = error_result(create_new)
                removal = error_result(lambda: os.unlink(path / 'original'))
                result.update(creation=creation, removal=removal,
                              denied=creation['denied'] and removal['denied'])
            elif kind == 'new-open':
                def write_new():
                    descriptor = os.open(path, os.O_WRONLY)
                    try:
                        os.write(descriptor, b'changed!')
                    finally:
                        os.close(descriptor)
                result.update(error_result(write_new))
            elif kind == 'existing-fd':
                result.update(error_result(lambda: os.write(writable, b'changed!')))
            elif kind == 'existing-mapping':
                child = os.fork()
                if child == 0:
                    try:
                        mapping[:] = b'changed!'
                        mapping.flush()
                        os._exit(0)
                    except (OSError, ValueError):
                        os._exit(80)
                status = wait_child(child)
                child = None
                result['writer_status'] = status
                result['denied'] = status in (80 << 8, signal.SIGBUS, signal.SIGSEGV)
            elif kind == 'pending-open':
                fcntl.fcntl(readonly, fcntl.F_SETLEASE, fcntl.F_UNLCK)
                lease = False
                status = wait_child(child)
                child = None
                result['writer_status'] = status
                result['denied'] = status == 80 << 8
            original = path / 'original' if kind == 'directory' else path
            result['original_bytes_preserved'] = original.exists() and original.read_bytes() == b'original'
            result['supported'] = bool(result.get('denied')
                                       and result['original_bytes_preserved'])
            return result
        finally:
            signal.alarm(0)
            signal.signal(signal.SIGALRM, previous_alarm)
            if child:
                try:
                    os.kill(child, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                os.waitpid(child, 0)
            if lease:
                fcntl.fcntl(readonly, fcntl.F_SETLEASE, fcntl.F_UNLCK)
            if mapping is not None:
                mapping.close()
            if writable is not None:
                os.close(writable)
            if flags(readonly) != original_flags:
                change(readonly, original_flags)
            os.close(readonly)
            signal.signal(signal.SIGIO, previous)


if __name__ == '__main__':
    if len(sys.argv) != 1:
        raise SystemExit('No paths or options are accepted; fixtures are allocated internally.')
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    capabilities = dict(line.split(':', 1) for line in
                        Path('/proc/self/status').read_text().splitlines() if ':' in line)
    if not int(capabilities['CapEff'].strip(), 16) & (1 << 9):
        print(json.dumps({'status': 'CAP_LINUX_IMMUTABLE-required',
                          'production_state_changed': False, 'grants_capacity': False}))
        raise SystemExit(3)
    observations = [run_case(kind) for kind in
                    ['directory', 'new-open', 'existing-fd', 'existing-mapping', 'pending-open']]
    print(json.dumps({'status': 'observed', 'observations': observations,
                      'production_state_changed': False, 'grants_capacity': False}))
    raise SystemExit(0 if all(row['supported'] for row in observations) else 3)
