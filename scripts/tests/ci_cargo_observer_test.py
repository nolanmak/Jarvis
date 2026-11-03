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


def lookup(uid,pid,tid,call,sequence,result,path,*,reverse=False):
    name=frame('printf',f'N {uid} {pid} {tid} {call} {sequence} {path}\n')
    outcome=frame('printf',f'E {uid} {pid} {tid} {call} {sequence} {result}\n')
    return outcome+name if reverse else name+outcome


class KernelObserverTests(unittest.TestCase):
    def test_startup_error_is_not_classified_from_an_unrelated_probe_warning(self):
        data=(b'stdin:6-7: WARNING: getname_flags is not traceable; attaching will likely fail\n'
            b'error: in function rawtracepoint:sys_exit: Looks like the BPF stack limit is exceeded.\n')
        self.assertEqual(observer.startup_errno(data),errno.E2BIG)
        self.assertEqual(observer.startup_errno(b'ERROR: No such file or directory'),errno.ENOENT)
        self.assertEqual(observer.startup_errno(b'WARNING: not found\nERROR: invalid argument'),errno.EINVAL)

    def test_counter_bound_kernel_records_keep_success_and_negative_queries(self):
        data=(lookup(1999,100,100,257,1,-2,'/ci-observer-witness-test')
            +lookup(2004,101,101,257,10,3,'/workspace/target/deps/input')
            +lookup(2004,101,101,257,12,-2,'/workspace/target/deps/missing')+frame('map',{'@emitted':6}))
        events=observer.decode(data)
        self.assertEqual(len(events),3)
        self.assertEqual([e['result'] for e in events],[-2,3,-2])
        self.assertEqual(events[2]['path'],b'/workspace/target/deps/missing')
        for bad in (data[:-1],data.replace(b'"@emitted": 6',b'"@emitted": 5'),
                data.replace(b'"@emitted": 6',b'"@emitted": 7'),data+frame('map',{'@emitted':6}),
                data.replace(b'2004 101',b'1001 101'),data.replace(b'257 10',b'999 10'),
                data.replace(b'101 101',b'0 0'),data.replace(b'/input',b'/raw-\xff'),
                data.replace(b'/input',b'/input\\nforged'),data.replace(b'/input',b'/input\\n'),
                data+frame('lost_events',{'events':1}),data+frame('map',{'@emitted':True})):
            with self.subTest(data=bad),self.assertRaises(observer.ObservationUnavailable):observer.decode(bad)

    def test_open_records_cannot_escape_the_selected_kernel_probe_namespace(self):
        for path in ('/etc/private','/workspace/target/deps/../other','/ci-observer-witness-bad;code'):
            with self.subTest(path=path),self.assertRaises(observer.ObservationUnavailable):
                observer.decode(lookup(2004,101,101,257,1,3,path)+frame('map',{'@emitted':2}))

    def test_directory_trailing_slash_is_preserved_without_accepting_empty_interior_components(self):
        for path in ('/workspace/directory','/workspace/directory/'):
            self.assertEqual(observer.decode(lookup(2004,101,101,262,1,0,path)
                +frame('map',{'@emitted':2}))[0]['path'],path.encode())
        for path in ('/workspace//directory','/workspace/directory//','/workspace/directory/../'):
            with self.subTest(path=path),self.assertRaises(observer.ObservationUnavailable):
                observer.decode(lookup(2004,101,101,262,1,0,path)+frame('map',{'@emitted':2}))

    def test_cross_cpu_delivery_keeps_repeated_thread_lookups_distinct(self):
        first=lookup(2004,101,101,257,10,3,'/workspace/first',reverse=True)
        second=lookup(2004,101,101,257,12,-2,'/workspace/second')
        # A result can arrive before its name on a different CPU buffer.
        events=observer.decode(first+second+frame('map',{'@emitted':4}))
        self.assertEqual([(e['path'],e['result']) for e in events],[(b'/workspace/first',3),(b'/workspace/second',-2)])
        records=(first+second).splitlines(keepends=True)
        for bad in (b''.join(records[:1]),b''.join(records[1:]),
                b''.join(records[:1]+records),b''.join(records).replace(b'257 10',b'257 0'),
                b''.join(records).replace(b'N 2004 101 101 257 12',b'N 2004 101 101 257 13')):
            with self.subTest(data=bad),self.assertRaises(observer.ObservationUnavailable):
                observer.decode(bad+frame('map',{'@emitted':len(bad.splitlines())}))
        self.assertEqual(observer.decode(records[0],complete=False),[])

    def test_truncated_and_unbounded_streams_have_no_observation(self):
        for value in (b'',frame('printf','E 2004 101 101 257 3 /workspace/target/deps/input\n'),b'x'*(4*1024**2+1)):
            with self.subTest(size=len(value)),self.assertRaises(observer.ObservationUnavailable):observer.decode(value)

    def test_path_at_capture_capacity_cannot_authorize_a_truncated_lookup(self):
        prefix='/workspace/'
        path=prefix+'x'*(127-len(prefix))
        data=lookup(2004,101,101,257,1,3,path)+frame('map',{'@emitted':2})
        with self.assertRaises(observer.ObservationUnavailable):observer.decode(data)
        shorter=path[:-1]
        self.assertEqual(observer.decode(lookup(2004,101,101,257,1,3,shorter)
            +frame('map',{'@emitted':2}))[0]['path'],shorter.encode())

    @unittest.skipIf(os.geteuid()==0,'non-root refusal is exercised by a non-root caller')
    def test_no_host_caller_can_start_the_privileged_kernel_probe(self):
        with self.assertRaises(observer.ObservationUnavailable):observer.Observer(None,deadline=0)


if __name__=='__main__':unittest.main()
