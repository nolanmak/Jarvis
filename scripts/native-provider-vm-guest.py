#!/usr/bin/env python3
"""Trusted guest supervisor; worker never receives the host control channel."""
import base64
import ctypes
import json
import os
from pathlib import Path
import resource
import shutil
import socket
import stat
import subprocess
import sys
import threading

MAX_FRAME = 2 * 1024 * 1024
MAX_HOME = 64 * 1024 * 1024


def worker(job):
    uid, gid = job['uid'], job['gid']
    if uid <= 0 or gid <= 0:
        raise ValueError('worker must be unprivileged')
    if ctypes.CDLL(None).prctl(38, 1, 0, 0, 0):
        raise RuntimeError('no-new-privileges unavailable')
    os.setgroups([])
    os.setgid(gid)
    os.setuid(uid)
    os.umask(0o077)
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    resource.setrlimit(resource.RLIMIT_NPROC, (128, 128))
    resource.setrlimit(resource.RLIMIT_NOFILE, (256, 256))
    resource.setrlimit(resource.RLIMIT_FSIZE, (MAX_HOME, MAX_HOME))
    os.chdir('/workspace')
    os.execve('/native/cli', ['/native/cli', *job['argv']], job['environment'])


def channel():
    for port in Path('/sys/class/virtio-ports').iterdir():
        if (port / 'name').read_text().strip() == 'org.jarvis.native.control':
            fd = os.open('/dev/' + port.name, os.O_RDWR | os.O_NOFOLLOW | os.O_CLOEXEC)
            info = os.fstat(fd)
            if not stat.S_ISCHR(info.st_mode) or info.st_uid != 0:
                os.close(fd)
                raise RuntimeError('invalid controller channel')
            os.fchmod(fd, 0o600)
            return fd
    raise RuntimeError('controller channel unavailable')


def export_home(send):
    total = 0
    for root, directories, files in os.walk('/home/worker', followlinks=False):
        for name in directories:
            if (Path(root) / name).is_symlink():
                raise ValueError('runtime directory symlink')
        for name in files:
            path = Path(root) / name
            fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
            with os.fdopen(fd, 'rb') as stream:
                info = os.fstat(stream.fileno())
                if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
                    raise ValueError('runtime file is not regular')
                total += info.st_size
                if total > MAX_HOME:
                    raise ValueError('runtime export exceeds budget')
                relative = str(path.relative_to('/home/worker'))
                send({'type': 'runtime_start', 'path': relative, 'size': info.st_size})
                offset = 0
                while data := stream.read(65536):
                    send({'type': 'runtime_data', 'offset': offset,
                          'data': base64.b64encode(data).decode()})
                    offset += len(data)
                if offset != info.st_size:
                    raise ValueError('runtime changed during export')
                send({'type': 'runtime_end'})


def main():
    sys.path.insert(0, '/root')
    from native_provider_gateway import GuestGateway
    job = json.loads(Path('/root/job.json').read_text())
    fd = channel()
    output = os.fdopen(os.dup(fd), 'wb', buffering=0)
    lock = threading.Lock()

    def send(frame):
        data = json.dumps(frame, separators=(',', ':')).encode() + b'\n'
        if len(data) > MAX_FRAME:
            raise ValueError('oversized guest frame')
        with lock:
            output.write(data)

    uid, gid = job['uid'], job['gid']
    if uid <= 0 or gid <= 0:
        raise ValueError('invalid worker identity')
    shutil.copytree('/seed', '/home/worker', dirs_exist_ok=True)
    for root, directories, files in os.walk('/home/worker'):
        os.chown(root, uid, gid)
        os.chmod(root, 0o700)
        for name in files:
            os.chown(Path(root) / name, uid, gid)
    os.chown('/workspace', uid, gid)
    Path('/etc/passwd').write_text(f'root:x:0:0:root:/root:/bin/sh\nworker:x:{uid}:{gid}:worker:/home/worker:/bin/sh\n')
    Path('/etc/group').write_text(f'root:x:0:\nworker:x:{gid}:\n')
    gateway = GuestGateway(job['gateway'], send)
    proxy = gateway.start()
    job['environment'].update({'HOME': '/home/worker', 'PATH': '/usr/bin:/bin',
        'TMPDIR': '/tmp', 'CODEX_HOME': '/home/worker/.codex',
        'CLAUDE_CONFIG_DIR': '/home/worker/.claude', 'HTTP_PROXY': proxy,
        'HTTPS_PROXY': proxy, 'http_proxy': proxy, 'https_proxy': proxy,
        'NO_PROXY': '', 'no_proxy': '', 'ALL_PROXY': '', 'all_proxy': '', 'SSL_CERT_FILE': job['gateway']['ca_file'],
        'NODE_EXTRA_CA_CERTS': job['gateway']['ca_file']})
    # One relay connection is an untrusted byte stream to the original host
    # policy bridge, not access to the gateway/control frame parser.
    relay = socket.socket(socket.AF_UNIX)
    relay.bind('/tmp/native-mcp.sock')
    os.chown('/tmp/native-mcp.sock', uid, gid)
    os.chmod('/tmp/native-mcp.sock', 0o600)
    relay.listen(1)
    relay_connection = []
    relay_ready = threading.Event()
    def relay_input():
        connection, _ = relay.accept()
        relay_connection.append(connection)
        relay_ready.set()
        while data := connection.recv(65536):
            send({'type': 'mcp_input', 'data': base64.b64encode(data).decode()})
    if job['mcp']:
        threading.Thread(target=relay_input, daemon=True).start()
    started = threading.Event()
    child = []
    def receive():
        with os.fdopen(fd, 'rb') as stream:
            for raw in iter(lambda: stream.readline(MAX_FRAME + 1), b''):
                if len(raw) > MAX_FRAME or not raw.endswith(b'\n'):
                    os._exit(126)
                frame = json.loads(raw)
                kind = frame['type']
                if kind == 'start' and not started.is_set():
                    started.set()
                elif kind.startswith('gateway_'):
                    gateway.receive(frame)
                elif kind in ('stdin', 'stdin_end'):
                    started.wait()
                    while not child:
                        threading.Event().wait(0.001)
                    if kind == 'stdin':
                        child[0].stdin.write(base64.b64decode(frame['data'], validate=True))
                        child[0].stdin.flush()
                    else:
                        child[0].stdin.close()
                elif kind in ('mcp_output', 'mcp_end') and job['mcp']:
                    if not relay_ready.wait(30):
                        os._exit(126)
                    if kind == 'mcp_output':
                        relay_connection[0].sendall(base64.b64decode(frame['data'], validate=True))
                    else:
                        relay_connection[0].shutdown(socket.SHUT_WR)
                else:
                    os._exit(126)
        os._exit(126)
    threading.Thread(target=receive, daemon=True).start()
    send({'type': 'ready'})
    if not started.wait(60):
        raise RuntimeError('controller admission not received')
    # Separate root helper drops privileges before exec; no preexec_fn in this
    # threaded supervisor and all control descriptors remain close-on-exec.
    Path('/root/worker.json').write_text(json.dumps(job))
    process = subprocess.Popen(['/usr/bin/python3', '-I', '-S', '/root/guest.py', '--worker'],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    child.append(process)
    def copy(stream, kind):
        while data := stream.read1(65536):
            send({'type': kind, 'data': base64.b64encode(data).decode()})
    threads = [threading.Thread(target=copy, args=(process.stdout, 'stdout')),
               threading.Thread(target=copy, args=(process.stderr, 'stderr'))]
    for thread in threads:
        thread.start()
    code = process.wait()
    # Native descendants cannot survive export; worker UID is unique in this
    # disposable VM. Root gateway and supervisor remain alive for export only.
    for entry in Path('/proc').iterdir():
        if entry.name.isdigit():
            try:
                if entry.stat().st_uid == uid:
                    os.kill(int(entry.name), 9)
            except (FileNotFoundError, ProcessLookupError):
                pass
    for thread in threads:
        thread.join(timeout=5)
        if thread.is_alive():
            raise RuntimeError('native output did not close')
    export_home(send)
    send({'type': 'exit', 'code': code if code >= 0 else 128 - code})


if __name__ == '__main__':
    if sys.argv[1:] == ['--worker']:
        worker(json.loads(Path('/root/worker.json').read_text()))
    else:
        try:
            main()
        except BaseException:
            os._exit(126)
