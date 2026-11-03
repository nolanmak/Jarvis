"""Kernel-copied pathname experiment; never a complete input/CI authority."""
import importlib.util
import errno
import json
import os
from pathlib import Path
import unittest

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('kernel_observer',ROOT/'ci-cargo-observer.py')
observer=importlib.util.module_from_spec(spec);spec.loader.exec_module(observer)


def frame(kind,data):return (json.dumps({'type':kind,'data':data})+'\n').encode()


class KernelObserverTests(unittest.TestCase):
    def test_startup_error_is_not_classified_from_an_unrelated_probe_warning(self):
        data=(b'stdin:6-7: WARNING: getname_flags is not traceable; attaching will likely fail\n'
            b'error: in function rawtracepoint:sys_exit: Looks like the BPF stack limit is exceeded.\n')
        self.assertEqual(observer.startup_errno(data),errno.E2BIG)
        self.assertEqual(observer.startup_errno(b'ERROR: No such file or directory'),errno.ENOENT)
        self.assertEqual(observer.startup_errno(b'WARNING: not found\nERROR: invalid argument'),errno.EINVAL)

    def test_counter_bound_kernel_records_keep_success_and_negative_queries(self):
        data=(frame('printf','E 1999 100 100 257 -2 /ci-observer-witness-test\n')
            +frame('printf','E 2004 101 101 257 3 /workspace/target/deps/input\n')
            +frame('printf','E 2004 101 101 257 -2 /workspace/target/deps/missing\n')+frame('map',{'@emitted':3}))
        events=observer.decode(data)
        self.assertEqual(len(events),3)
        self.assertEqual([e['result'] for e in events],[-2,3,-2])
        self.assertEqual(events[2]['path'],b'/workspace/target/deps/missing')
        for bad in (data[:-1],data.replace(b'"@emitted": 3',b'"@emitted": 2'),
                data.replace(b'"@emitted": 3',b'"@emitted": 4'),data+frame('map',{'@emitted':3}),
                data.replace(b'2004 101',b'1001 101'),data.replace(b'257 3',b'999 3'),
                data.replace(b'101 101',b'0 0'),data.replace(b'/input',b'/raw-\xff'),
                data.replace(b'/input',b'/input\\nforged'),data.replace(b'/input',b'/input\\n'),
                data+frame('lost_events',{'events':1}),data+frame('map',{'@emitted':True})):
            with self.subTest(data=bad),self.assertRaises(observer.ObservationUnavailable):observer.decode(bad)

    def test_open_records_cannot_escape_the_selected_kernel_probe_namespace(self):
        for path in ('/etc/private','/workspace/target/deps/../other','/ci-observer-witness-bad;code'):
            with self.subTest(path=path),self.assertRaises(observer.ObservationUnavailable):
                observer.decode(frame('printf','E 2004 101 101 257 3 '+path+'\n')+frame('map',{'@emitted':1}))

    def test_truncated_and_unbounded_streams_have_no_observation(self):
        for value in (b'',frame('printf','E 2004 101 101 257 3 /workspace/target/deps/input\n'),b'x'*(4*1024**2+1)):
            with self.subTest(size=len(value)),self.assertRaises(observer.ObservationUnavailable):observer.decode(value)

    def test_path_at_capture_capacity_cannot_authorize_a_truncated_lookup(self):
        prefix='/workspace/'
        path=prefix+'x'*(95-len(prefix))
        data=frame('printf','E 2004 101 101 257 3 '+path+'\n')+frame('map',{'@emitted':1})
        with self.assertRaises(observer.ObservationUnavailable):observer.decode(data)
        shorter=path[:-1]
        self.assertEqual(observer.decode(frame('printf','E 2004 101 101 257 3 '+shorter+'\n')
            +frame('map',{'@emitted':1}))[0]['path'],shorter.encode())

    @unittest.skipIf(os.geteuid()==0,'non-root refusal is exercised by a non-root caller')
    def test_no_host_caller_can_start_the_privileged_kernel_probe(self):
        with self.assertRaises(observer.ObservationUnavailable):observer.Observer(None,deadline=0)


if __name__=='__main__':unittest.main()
