"""Actual unprivileged relay dispatch, cleanup and protocol refusal."""
import array
import fcntl
import importlib.util
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('cargo_relay', ROOT / 'ci-cargo-relay.py')
relay = importlib.util.module_from_spec(spec)
spec.loader.exec_module(relay)


class RelayTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory(prefix='jcr-', dir='/tmp')))
        self.root.chmod(0o700)
        self.server = self.enterContext(relay.Relay(self.root, timeout=5))

    def request(self, argv, *, environment=None, token=None, seal=True, extras=()):
        environment = environment if environment is not None else {b'PATH': os.fsencode(os.defpath)}
        payload = relay.encode_request(os.fsencode(argv[0]), list(map(os.fsencode, argv)),
            [key + b'=' + value for key, value in environment.items()], os.fsencode(self.root))
        fd = os.memfd_create('relay-test', os.MFD_CLOEXEC | os.MFD_ALLOW_SEALING)
        self.addCleanup(os.close, fd)
        os.write(fd, payload)
        if seal: fcntl.fcntl(fd, fcntl.F_ADD_SEALS, relay.SEALS)
        connection = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
        connection.settimeout(6)
        connection.connect(str(self.server.socket_path))
        self.addCleanup(connection.close)
        null = os.open('/dev/null', os.O_RDWR | os.O_CLOEXEC)
        self.addCleanup(os.close, null)
        numbers = [0, 1, 2] + [number for number, _ in extras]
        descriptors = [fd, null, null, null] + [descriptor for _, descriptor in extras]
        header = relay.request_header(self.server.token if token is None else token, numbers)
        connection.sendmsg([header], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array('i', descriptors))])
        return connection

    def frame(self, connection):
        return relay.decode_response(connection.recv(64))

    def test_real_exit_cwd_and_environment_follow_original_cleanup(self):
        effect = self.root / 'effect'
        code = 'import os;from pathlib import Path;Path("effect").write_bytes(os.environb[b"VALUE"]);raise SystemExit(17)'
        connection = self.request([sys.executable, '-I', '-c', code], environment={b'VALUE': b'\xffdata'})
        self.assertEqual(self.frame(connection), ('started', 0))
        self.assertEqual(self.frame(connection), ('finished', 17))
        self.assertEqual(effect.read_bytes(), b'\xffdata')
        self.assertEqual(len(self.server.completed), 1)
        receipt = self.server.completed[0]
        self.assertEqual(receipt['cleanup'], 'all-descendants-reaped')
        self.assertFalse(receipt['cargo_metadata_isolation_established'])
        self.assertFalse(receipt['shipping_authority'])

    def test_failed_exec_never_sends_started(self):
        connection = self.request([str(self.root / 'missing-tool')])
        self.assertEqual(self.frame(connection), ('error', 2))
        self.assertEqual(self.server.completed[0]['cleanup'], 'all-descendants-reaped')

    def test_unsealed_payload_wrong_token_and_regular_extra_fd_refuse_before_launch(self):
        for kind in ('unsealed', 'token', 'regular-fd'):
            with self.subTest(kind=kind):
                effect = self.root / kind
                fd = os.open(self.root / 'regular', os.O_CREAT | os.O_RDWR, 0o600)
                try:
                    connection = self.request([sys.executable, '-I', '-c', f'open({str(effect)!r},"w").write("ran")'],
                        seal=kind != 'unsealed', token=b'x' * 32 if kind == 'token' else None,
                        extras=((50, fd),) if kind == 'regular-fd' else ())
                    self.assertEqual(self.frame(connection)[0], 'error')
                    self.assertFalse(effect.exists())
                finally: os.close(fd)

    def test_disconnect_reaps_detached_writer_before_completing(self):
        marker = self.root / 'late-write'
        ready = self.root / 'ready'
        release = self.root / 'release'
        code = ('import os,time;from pathlib import Path;pid=os.fork();'
            'os.setsid() if pid==0 else None;'
            f'Path({str(ready)!r}).write_text("ready") if pid==0 else None\n'
            f'while not Path({str(release)!r}).exists():time.sleep(.005)\n'
            f'Path({str(marker)!r}).write_text("late") if pid==0 else None')
        connection = self.request([sys.executable, '-I', '-c', code])
        self.assertEqual(self.frame(connection), ('started', 0))
        deadline = time.monotonic() + 3
        while not ready.exists() and time.monotonic() < deadline: time.sleep(.005)
        self.assertTrue(ready.exists())
        connection.close()
        deadline = time.monotonic() + 4
        while not self.server.completed and time.monotonic() < deadline: time.sleep(.005)
        self.assertTrue(self.server.completed, 'disconnect cleanup was not acknowledged')
        self.assertEqual(self.server.completed[0]['cleanup'], 'all-descendants-reaped')
        release.write_text('after cleanup')
        time.sleep(.03)
        self.assertFalse(marker.exists())

    def test_jobserver_fifo_is_forwarded_under_original_number(self):
        read, write = os.pipe()
        try:
            os.write(write, b'+')
            effect = self.root / 'jobserver'
            connection = self.request([sys.executable, '-I', '-c',
                f'import os;open({str(effect)!r},"wb").write(os.read(50,1))'], extras=((50, read),))
            self.assertEqual(self.frame(connection), ('started', 0))
            self.assertEqual(self.frame(connection), ('finished', 0))
            self.assertEqual(effect.read_bytes(), b'+')
        finally:
            os.close(read); os.close(write)


if __name__ == '__main__':
    unittest.main()
