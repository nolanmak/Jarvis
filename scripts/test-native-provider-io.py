#!/usr/bin/env python3
"""Local pipe/proc fixtures; never boot a VM or signal host worker processes."""
import importlib.util
import os
from pathlib import Path
import tempfile
import threading
import time
import unittest


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


HOST = load('host_native_io', 'native-provider-vm.py')
GUEST = load('guest_native_io', 'native-provider-vm-guest.py')


class NativeIoTests(unittest.TestCase):
    def test_full_application_pipe_never_blocks_control_dispatcher(self):
        for module in (HOST, GUEST):
            with self.subTest(module=module.__name__):
                read_fd, write_fd = os.pipe()
                os.set_blocking(write_fd, False)
                try:
                    while True:
                        os.write(write_fd, b'full' * 16384)
                except BlockingIOError:
                    pass
                os.set_blocking(write_fd, True)
                entered, failed = threading.Event(), threading.Event()
                def write(data):
                    entered.set()
                    remaining = memoryview(data)
                    while remaining:
                        remaining = remaining[os.write(write_fd, remaining):]
                writer = module.BoundedWriter(write, lambda: None, failed.set)
                try:
                    writer.submit(b'x' * 65536)
                    self.assertTrue(entered.wait(1))
                    # A real full pipe blocks the dedicated writer. The thread
                    # dispatching control/gateway frames can still process work.
                    started = time.monotonic()
                    for _ in range(16):
                        writer.submit(b'y')
                    with self.assertRaisesRegex(RuntimeError, 'backpressure'):
                        writer.submit(b'overflow')
                    self.assertLess(time.monotonic() - started, 0.5)
                    self.assertFalse(writer.closed.is_set())
                finally:
                    os.close(read_fd)
                    writer.thread.join(timeout=2)
                    os.close(write_fd)
                self.assertTrue(failed.is_set())
                self.assertTrue(writer.closed.is_set())

    def test_ordered_bytes_and_eof_are_preserved(self):
        for module in (HOST, GUEST):
            output = []
            failed = threading.Event()
            writer = module.BoundedWriter(output.append, lambda: output.append(None), failed.set)
            writer.submit(b'first')
            writer.submit(b'second')
            writer.submit(None)
            writer.thread.join(timeout=1)
            self.assertEqual(output, [b'first', b'second', None])
            self.assertFalse(failed.is_set())
            with self.assertRaises(RuntimeError):
                writer.submit(b'after eof')

    def test_quiescence_repeats_after_late_fork_and_reaps_before_export(self):
        generations = iter([[101], [102], [], []])
        events = []
        GUEST.quiesce_worker(12345, timeout=1,
            enumerate_pids=lambda: next(generations),
            kill=lambda pid: events.append(('kill', pid)),
            reap=lambda: events.append(('reap', None)))
        events.append(('export', None))
        self.assertEqual([item for item in events if item[0] == 'kill'], [('kill', 101), ('kill', 102)])
        self.assertEqual(events[-3:], [('reap', None), ('reap', None), ('export', None)])

    def test_live_worker_prevents_runtime_export(self):
        exported = []
        with self.assertRaisesRegex(RuntimeError, 'export refused'):
            GUEST.quiesce_worker(12345, timeout=0.03,
                enumerate_pids=lambda: [101], kill=lambda _: None, reap=lambda: None)
            exported.append(True)
        self.assertEqual(exported, [])

    def test_nondumpable_worker_is_found_using_kernel_uid_fields(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for pid, uid in [(101, 12345), (102, 0)]:
                (root / str(pid)).mkdir()
                (root / str(pid) / 'status').write_text(f'Name:\tfixture\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n')
            # Fixture proc directories belong to the test runner, not12345.
            # A dumpability-driven inode ownership change must not hide UID.
            self.assertEqual(GUEST.worker_pids(12345, root), [101])


if __name__ == '__main__':
    unittest.main()
