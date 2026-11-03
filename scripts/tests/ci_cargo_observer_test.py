"""Kernel-copied pathname experiment; never a complete input/CI authority."""
import importlib.util
import copy
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
    name=b''.join(frame('printf',f'N {uid} {pid} {tid} {call} {sequence} {offset} {len(path)} {path[offset:offset+63]}\n')
        for offset in range(0,len(path),63))
    outcome=frame('printf',f'E {uid} {pid} {tid} {call} {sequence} {result}\n')
    return outcome+name if reverse else name+outcome


def census_sample():
    programs=[{'id':i,'type':17 if i in (1,4) else 2,'tag':str(i)*16,'recursion_misses':0} for i in range(1,5)]
    links=[{'id':10+i,'type':'raw_tracepoint' if i in (1,4) else 'kprobe_multi',
        'prog_id':i,'tag':str(i)*16,**({'tracepoint':'sys_enter' if i==1 else 'sys_exit'} if i in (1,4)
            else {'function':'getname_flags','count':1,'flags':1,'missed':0})} for i in range(1,5)]
    return {'pid':42,'birth':1000,'programs':programs,'links':links}


class KernelObserverTests(unittest.TestCase):
    def test_only_the_same_four_original_links_and_programs_with_zero_misses_have_a_census(self):
        sample=census_sample();value={'before':sample,'after':copy.deepcopy(sample)}
        observer.validate_census(value)
        for fault in ('missed-before','missed-after','recursion','missing-link','duplicate-program',
                'changed-owner','changed-birth','changed-link','changed-tag','wrong-symbol','multiple-functions',
                'entry-probe','foreign-raw-event','bool-misses','unknown-fields','absent-counter'):
            bad=copy.deepcopy(value);before=bad['before'];after=bad['after']
            if fault=='missed-before':before['links'][1]['missed']=1
            elif fault=='missed-after':after['links'][1]['missed']=1
            elif fault=='recursion':after['programs'][0]['recursion_misses']=1
            elif fault=='missing-link':after['links'].pop()
            elif fault=='duplicate-program':after['programs'][1]=after['programs'][0]
            elif fault=='changed-owner':after['pid']+=1
            elif fault=='changed-birth':after['birth']+=1
            elif fault=='changed-link':after['links'][1]['id']+=100
            elif fault=='changed-tag':after['links'][1]['tag']='a'*16
            elif fault=='wrong-symbol':after['links'][1]['function']='other'
            elif fault=='multiple-functions':after['links'][1]['count']=2
            elif fault=='entry-probe':after['links'][1]['flags']=0
            elif fault=='foreign-raw-event':after['links'][0]['tracepoint']='sched_switch'
            elif fault=='bool-misses':after['links'][1]['missed']=False
            elif fault=='unknown-fields':after['links'][1]['unknown']=True
            elif fault=='absent-counter':del after['programs'][0]['recursion_misses']
            with self.subTest(fault=fault),self.assertRaises(observer.ObservationUnavailable):observer.validate_census(bad)

    def test_fixed_loader_summaries_never_replace_nonce_events_or_hide_unknown_output(self):
        logs=(b'\nAttaching to 1 functions\nProgram ID: 4\n\nThe verifier log: \n'
            b'processed 298 insns (limit 1000000) max_states_per_insn 1 total_states 24 peak_states 24 mark_read 0\n')
        data=lookup(2004,101,101,257,1,3,'/workspace/input')+frame('map',{'@emitted':2})
        self.assertEqual(len(observer.decode(logs+data)),1)
        for value in (logs,logs*17+data,b'unknown loader output\n'+data,
                b'Program ID: 4 forged\n'+data,b'The verifier log: unverified\n'+data,b'Attaching to 2 functions\n'+data):
            with self.subTest(value=value),self.assertRaises(observer.ObservationUnavailable):observer.decode(value)

    def test_expected_empty_map_default_does_not_hide_read_or_update_failures(self):
        normal={'type':'helper_error','msg':'Map lookup failed','helper':'map_lookup_elem','retcode':0,'line':5,'col':2}
        data=lookup(2004,101,101,257,1,3,'/workspace/input')+frame('map',{'@emitted':2})
        self.assertEqual(len(observer.decode((json.dumps(normal)+'\n').encode()+data)),1)
        for change in ({'helper':'probe_read_kernel','retcode':-14},{'helper':'map_update_elem','retcode':-12},
                {'helper':'map_delete_elem','retcode':-2},{'retcode':True},{'line':0},{'extra':'unknown'}):
            with self.subTest(change=change),self.assertRaises(observer.ObservationUnavailable):
                observer.decode((json.dumps({**normal,**change})+'\n').encode()+data)

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
        path=prefix+'x'*(4096-len(prefix))
        records=lookup(2004,101,101,257,1,3,path)
        data=records+frame('map',{'@emitted':len(records.splitlines())})
        with self.assertRaises(observer.ObservationUnavailable):observer.decode(data)
        shorter=path[:-1]
        records=lookup(2004,101,101,257,1,3,shorter)
        self.assertEqual(observer.decode(records+frame('map',{'@emitted':len(records.splitlines())}))[0]['path'],shorter.encode())

    def test_long_copied_names_require_all_exact_offset_chunks_and_one_result(self):
        path='/workspace/'+('long-component/'*16)+'generated.rs'
        records=lookup(2004,101,101,257,100,3,path).splitlines(keepends=True)
        count=lambda rows: b''.join(rows)+frame('map',{'@emitted':len(rows)})
        self.assertEqual(observer.decode(count(records[::-1]))[0]['path'],path.encode())
        for rows in (records[1:],records+records[:1],
                [records[0].replace(b' 0 ',b' 1 ')]+records[1:],
                [records[0].replace(path[:63].encode(),path[:62].encode())]+records[1:]):
            with self.subTest(rows=rows),self.assertRaises(observer.ObservationUnavailable):observer.decode(count(rows))

    @unittest.skipIf(os.geteuid()==0,'non-root refusal is exercised by a non-root caller')
    def test_no_host_caller_can_start_the_privileged_kernel_probe(self):
        with self.assertRaises(observer.ObservationUnavailable):observer.Observer(None,deadline=0)


if __name__=='__main__':unittest.main()
