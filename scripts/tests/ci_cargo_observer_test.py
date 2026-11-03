"""Kernel-copied pathname experiment; never a complete input/CI authority."""
import importlib.util
import copy
import errno
import json
import os
from pathlib import Path
import unittest
from types import SimpleNamespace

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('kernel_observer',ROOT/'ci-cargo-observer.py')
observer=importlib.util.module_from_spec(spec);spec.loader.exec_module(observer)


def frame(kind,data):return (json.dumps({'type':kind,'data':data})+'\n').encode()


def lookup(uid,pid,tid,call,sequence,result,path,*,reverse=False):
    path=os.fsencode(path)
    name=b''.join(frame('printf',f'N {uid} {pid} {tid} {call} {sequence} {offset} {len(path)} '+path[offset:offset+63].hex(' ')+'\n')
        for offset in range(0,len(path),63))
    outcome=frame('printf',f'E {uid} {pid} {tid} {call} {sequence} {result}\n')
    return outcome+name if reverse else name+outcome


def census_sample():
    programs=[{'id':i,'type':17 if i in (1,4) else 2,'tag':f'{i:016x}','recursion_misses':0} for i in range(1,5)]
    links=[{'id':10+i,'type':'raw_tracepoint' if i in (1,4) else 'kretprobe_multi',
        'prog_id':i,'tag':f'{i:016x}',**({'tracepoint':'sys_enter' if i==1 else 'sys_exit'} if i in (1,4)
            else {'function':'getname_flags','count':1,'flags':1,'missed':0})} for i in range(1,5)]
    for i,(function,flags) in enumerate((('iterate_dir',0),('iterate_dir',1),('filldir64',0),('filldir64',1),('vfs_fstat',0),('vfs_fstat',1),('cp_new_stat',0),('cp_new_stat',1),('cp_statx',0),('cp_statx',1),('path_init',0),('path_init',1)),5):
        programs.append({'id':i,'type':2,'tag':f'{i:016x}','recursion_misses':0})
        links.append({'id':10+i,'type':'kretprobe_multi' if flags else 'kprobe_multi',
            'prog_id':i,'tag':f'{i:016x}','function':function,'count':1,'flags':flags,'missed':0})
    return {'pid':42,'birth':1000,'programs':programs,'links':links}


class KernelObserverTests(unittest.TestCase):
    def test_lookup_base_keeps_attempts_objects_failures_and_original_query_binding(self):
        rows=['Q 2004 101 101 262 1 1 3 256 50 100','R 2004 101 101 262 1 1 0',
            'C 2004 101 101 262 1 1 7 900 16749 4096 8192 4 6 6 256 4',
            'Q 2004 101 101 262 1 2 3 0 50 100','R 2004 101 101 262 1 2 -9']
        attempts=observer.lookup_bases(rows)
        self.assertEqual([a['attempt'] for a in attempts],[1,2])
        self.assertEqual(attempts[0]['base']['inode'],900)
        self.assertEqual(attempts[0]['base']['inode_generation'],4)
        self.assertEqual(attempts[0]['base']['path_seq'],6)
        self.assertEqual(attempts[0]['mount_namespace'],50)
        self.assertIsNone(attempts[1]['base'])
        data=(lookup(2004,101,101,262,1,-2,'src/main.rs')
            +b''.join(frame('printf',r+'\n') for r in rows)+frame('map',{'@emitted':len(rows)+2}))
        self.assertEqual(observer.decode(data)[0]['lookup_attempts'],attempts)
        event=observer.decode(data)[0];event['lookup_attempts']=attempts[:1]
        expected=SimpleNamespace(stat=lambda:SimpleNamespace(st_dev=os.makedev(0,7),st_ino=900,st_mode=16749))
        observer.lookup_base_witness([event],101,2004,b'src/main.rs',262,-2,dfd=3,
            user_namespace=100,mount_namespace=50,expected=expected)
        for fault in ('namespace','sequence','inode','descriptor'):
            bad=copy.deepcopy(event)
            if fault=='namespace':bad['lookup_attempts'][0]['mount_namespace']=51
            elif fault=='sequence':bad['lookup_attempts'][0]['base']['dentry_seq']=8
            elif fault=='inode':bad['lookup_attempts'][0]['base']['inode']=901
            else:bad['lookup_attempts'][0]['dfd']=-100
            with self.subTest(fault=fault),self.assertRaises(observer.ObservationUnavailable):
                observer.lookup_base_witness([bad],101,2004,b'src/main.rs',262,-2,dfd=3,
                    user_namespace=100,mount_namespace=50,expected=expected)
        for bad in (rows[:-1],rows+rows[:1],rows[:2]+rows[3:],
                [r.replace('1 1 3','1 3 3') for r in rows],
                [r.replace('1 1 7 900','1 1 -1 900') for r in rows],
                [r.replace('1 2 -9','1 2 0') for r in rows]):
            with self.subTest(rows=bad),self.assertRaises(observer.ObservationUnavailable):observer.lookup_bases(bad)
        self.assertEqual(observer.lookup_bases(rows[:1],complete=False),[])

    def test_path_metadata_keeps_original_arguments_final_statx_fields_and_copy_result(self):
        rows=['U 2004 101 101 1 332 3 3 256 256 8191 8191 100','P 2004 101 101 1 1 100',
            'J 2004 101 101 1 7 900 33060 1 0 0 0 6 4096 8 8191',
            'K 2004 101 101 1 1 2 3 4 5 6',
            'O 2004 101 101 1 9223372036854775808 1 7 8 999 10 11 512 4096 512 1 2 3 4',
            'Y 2004 101 101 1 0','Z 2004 101 101 1 0',
            'U 2004 101 101 2 262 3 3 0 0 0 0 100','Z 2004 101 101 2 -2']
        queries=observer.metadata_queries(rows)
        self.assertEqual([q['syscall'] for q in queries],[332,262])
        self.assertEqual(queries[0]['flags'],256);self.assertEqual(queries[0]['mask'],8191)
        self.assertEqual(queries[0]['stat']['mnt_id'],999)
        self.assertEqual(queries[0]['stat']['attributes'],2**63)
        self.assertIsNone(queries[1]['stat']);self.assertIsNone(queries[1]['copy_result'])
        user=observer.statx_bytes(queries[0]['stat'])
        self.assertEqual(len(user),256)
        self.assertEqual(int.from_bytes(user[8:16],'little'),0)  # Kernel-only flag is masked by cp_statx.
        self.assertEqual(int.from_bytes(user[144:152],'little'),999)
        framed=(lookup(2004,101,101,332,1,0,'src/lib.rs')
            +lookup(2004,101,101,262,2,-2,'/workspace/missing')
            +b''.join(frame('printf',row+'\n') for row in rows)
            +frame('map',{'@emitted':len(rows)+4}))
        paths,dirs,observed=observer.decode(framed,with_metadata=True)
        self.assertEqual([q['path'] for q in observed],[b'src/lib.rs',b'/workspace/missing'])
        with self.assertRaises(observer.ObservationUnavailable):
            observer.decode(framed.replace(b'332 1 0',b'332 1 -14'))
        for bad in (rows[:-1],rows[:4]+rows[5:],rows+rows[:1],
                [r.replace('332 3 3','332 3 4') for r in rows],
                [r.replace('1 1 100','1 0 100') for r in rows]):
            with self.subTest(rows=bad),self.assertRaises(observer.ObservationUnavailable):observer.metadata_queries(bad)

    def test_fstat_keeps_original_descriptor_metadata_namespace_and_delivery_outcome(self):
        rows=['F 2004 101 101 1 3 3','H 2004 101 101 1 3 100',
            'J 2004 101 101 1 7 900 33060 1 0 0 0 6 4096 8 2047',
            'K 2004 101 101 1 -1 999999999 2 3 4 5','L 2004 101 101 1 0','Z 2004 101 101 1 0',
            'F 2004 101 101 2 18446744073709551615 -1','H 2004 101 101 2 -1 100',
            'L 2004 101 101 2 -9','Z 2004 101 101 2 -9']
        queries=observer.metadata_queries(rows[::-1])
        queries.sort(key=lambda q:q['sequence'])
        self.assertEqual([q['fd'] for q in queries],[3,-1])
        self.assertEqual(queries[0]['stat']['atime_sec'],-1)
        self.assertEqual(queries[0]['stat']['atime_nsec'],999999999)
        self.assertEqual(queries[0]['user_namespace'],100)
        self.assertTrue(queries[0]['delivered']);self.assertIsNone(queries[1]['stat'])
        fault=observer.metadata_queries([r.replace('1 0','1 -14') if r.startswith('Z ') else r for r in rows])
        self.assertFalse(next(q for q in fault if q['sequence']==1)['delivered'])
        path=SimpleNamespace(stat=lambda:SimpleNamespace(st_dev=os.makedev(0,7),st_ino=900,
            st_mode=33060,st_nlink=1,st_uid=0,st_gid=0,st_rdev=0,st_size=6,st_blksize=4096,
            st_blocks=8,st_atime_ns=-1,st_mtime_ns=2000000003,st_ctime_ns=4000000005))
        observer.metadata_witness(queries,101,2004,path,100)
        for bad,namespace in ((fault,100),(queries,101)):
            with self.subTest(namespace=namespace),self.assertRaises(observer.ObservationUnavailable):
                observer.metadata_witness(bad,101,2004,path,namespace)
        framed=b''.join(frame('printf',row+'\n') for row in rows)+frame('map',{'@emitted':len(rows)})
        paths,dirs,stats=observer.decode(framed,with_metadata=True)
        self.assertEqual((paths,dirs),([],[]));self.assertEqual(stats,queries)
        with self.assertRaises(observer.ObservationUnavailable):
            observer.decode(framed.replace(b'"@emitted": 10',b'"@emitted": 9'))
        self.assertEqual(observer.metadata_queries(rows[:1],complete=False),[])
        for bad in (rows[:-1],rows+rows[:1],rows[:2]+rows[3:],
                [r.replace('1 3 100','1 4 100') for r in rows],
                [r.replace('999999999','1000000000') for r in rows],
                [r.replace('1 7 900','1 -1 900') for r in rows],
                [r.replace('1 0','1 -9') if r.startswith('L ') else r for r in rows]):
            with self.subTest(rows=bad),self.assertRaises(observer.ObservationUnavailable):observer.metadata_queries(bad)

    def test_original_directory_cursor_records_rewind_and_refuses_a_false_continuous_scan(self):
        rows=['B 2004 101 101 1 3 24','I 2004 101 101 1 265289728 900 16749 0',
            'D 2004 101 101 1 1 900 4 0 0 1 2e','A 2004 101 101 1 1 1',
            'D 2004 101 101 1 2 901 4 1 0 2 2e 2e','A 2004 101 101 1 2 0',
            'T 2004 101 101 1 0 1','G 2004 101 101 1 24',
            'B 2004 101 101 2 3 288','I 2004 101 101 2 265289728 900 16749 0',
            'D 2004 101 101 2 1 900 4 0 0 1 2e','A 2004 101 101 2 1 1',
            'D 2004 101 101 2 2 901 4 1 0 2 2e 2e','A 2004 101 101 2 2 1',
            'T 2004 101 101 2 0 2','G 2004 101 101 2 48',
            'B 2004 101 101 3 3 288','I 2004 101 101 3 265289728 900 16749 2',
            'T 2004 101 101 3 0 2','G 2004 101 101 3 0']
        queries=observer.directory_queries(rows)
        path=SimpleNamespace(stat=lambda:SimpleNamespace(st_dev=os.makedev(253,0),st_ino=900),
            parent=SimpleNamespace(stat=lambda:SimpleNamespace(st_ino=901)))
        self.assertEqual([q['start_position'] for q in queries],[0,0,2])
        with self.assertRaises(observer.ObservationUnavailable):observer.directory_witness(queries,101,2004,path,{})
        self.assertEqual(observer.directory_cursor_witness(queries,101,2004,path),
            {'reads':3,'rewind_from':1,'rewind_to':0})
        for index,field,value in ((0,'start_position',1),(1,'start_position',1),
                (2,'start_position',99),(0,'position',0),(1,'fd',9)):
            bad=copy.deepcopy(queries);bad[index][field]=value
            with self.subTest(field=field,index=index),self.assertRaises(observer.ObservationUnavailable):
                observer.directory_cursor_witness(bad,101,2004,path)
        with self.assertRaises(observer.ObservationUnavailable):observer.directory_cursor_witness(queries[:-1],101,2004,path)

    def test_kernel_path_chunks_keep_binary_whitespace_and_control_bytes(self):
        path=b'/workspace/raw-\xff \n\x01'
        data=(frame('printf',f'N 2004 101 101 257 1 0 {len(path)} '+path.hex(' ')+'\n')
            +frame('printf','E 2004 101 101 257 1 3\n')+frame('map',{'@emitted':2}))
        self.assertEqual(observer.decode(data)[0]['path'],path)
        for bad in (data.replace(b'ff 20',b'ff'),data.replace(b'ff 20',b'00 20'),
                data.replace(b'ff 20',b'FF 20'),data.replace(b'ff 20',b'ff  20')):
            with self.subTest(data=bad),self.assertRaises(observer.ObservationUnavailable):observer.decode(bad)

    def test_directory_identity_compares_linux_kernel_device_encoding_to_stat(self):
        rows=['B 2004 101 101 1 3 64','I 2004 101 101 1 265289728 900 16749 0',
            'D 2004 101 101 1 1 900 4 0 0 1 2e','A 2004 101 101 1 1 1',
            'D 2004 101 101 1 2 901 4 1 0 2 2e 2e','A 2004 101 101 1 2 1',
            'T 2004 101 101 1 0 2','G 2004 101 101 1 48',
            'B 2004 101 101 2 3 64','I 2004 101 101 2 265289728 900 16749 2',
            'T 2004 101 101 2 0 2','G 2004 101 101 2 0']
        queries=observer.directory_queries(rows)
        path=SimpleNamespace(stat=lambda:SimpleNamespace(st_dev=os.makedev(253,0),st_ino=900),
            parent=SimpleNamespace(stat=lambda:SimpleNamespace(st_ino=901)))
        self.assertEqual(observer.directory_witness(queries,101,2004,path,{}),
            {'reads':2,'entries':2,'rejected_attempts':0})
        self.assertEqual(queries[0]['device'],265289728)  # Keep the raw kernel value.
        for bad in (queries[:-1],queries+queries,queries[1:],
                [{**q,'device':os.makedev(253,0)} for q in queries]):
            with self.subTest(queries=bad),self.assertRaises(observer.ObservationUnavailable):
                observer.directory_witness(bad,101,2004,path,{})

    def test_directory_capture_reconciles_kernel_entries_short_reads_and_eof(self):
        rows=['B 2004 101 101 50 3 64','I 2004 101 101 50 7 900 16749 0',
            'D 2004 101 101 50 1 900 4 0 0 1 2e','A 2004 101 101 50 1 1',
            'D 2004 101 101 50 2 901 4104 1 0 2 ff 61','A 2004 101 101 50 2 1',
            'D 2004 101 101 50 3 902 8 2 0 1 62','A 2004 101 101 50 3 0',
            'T 2004 101 101 50 0 2','G 2004 101 101 50 48',
            'B 2004 101 101 51 3 64','I 2004 101 101 51 7 900 16749 2',
            'T 2004 101 101 51 0 2','G 2004 101 101 51 0']
        queries=observer.directory_queries(rows)
        self.assertEqual([q['result'] for q in queries],[48,0])
        self.assertEqual([e['name'] for e in queries[0]['entries']],[b'.',b'\xffa'])
        self.assertEqual([e['returned_offset'] for e in queries[0]['entries']],[1,2])
        self.assertEqual(queries[0]['entries'][1]['raw_type'],4104)
        self.assertEqual(queries[0]['entries'][1]['type'],8)
        self.assertEqual(len(queries[0]['attempts']),3)
        self.assertFalse(queries[0]['attempts'][2]['accepted'])
        for bad in (rows[:-1],rows[:3]+rows[4:],rows+rows[:1],
                [r.replace('50 48','50 56') for r in rows],
                [r.replace('50 3 0','50 3 2') for r in rows],
                [r.replace('ff 61','ff') for r in rows],
                [r.replace('2 901 4104','2 902 4104') for r in rows]+[rows[4]],
                [r.replace('51 0','51 24') for r in rows]):
            with self.subTest(rows=bad),self.assertRaises(observer.ObservationUnavailable):observer.directory_queries(bad)
        self.assertEqual(observer.directory_queries(rows[:1],complete=False),[])
        framed=b''.join(frame('printf',row+'\n') for row in rows)+frame('map',{'@emitted':len(rows)})
        paths,dirs=observer.decode(framed,with_directories=True)
        self.assertEqual(paths,[]);self.assertEqual(dirs,queries)
        with self.assertRaises(observer.ObservationUnavailable):
            observer.decode(framed.replace(b'"@emitted": 14',b'"@emitted": 13'))

    def test_directory_binary_name_chunks_require_exact_coverage_even_when_reordered(self):
        name=b'\xff'*255
        rows=['B 2004 101 101 1 3 288','I 2004 101 101 1 7 900 16749 0',
            'A 2004 101 101 1 1 1','T 2004 101 101 1 0 99','G 2004 101 101 1 280']
        chunks=[f'D 2004 101 101 1 1 901 8 0 {start} 255 '+name[start:start+63].hex(' ')
            for start in range(0,len(name),63)]
        result=observer.directory_queries((rows+chunks)[::-1])[0]
        self.assertEqual(result['entries'][0]['name'],name)
        self.assertEqual(result['entries'][0]['returned_offset'],99)
        for bad in (rows+chunks[:-1],rows+chunks+chunks[:1],
                rows+[c.replace('901 8','902 8') if i else c for i,c in enumerate(chunks)],
                rows+[chunks[0].replace('ff ff','00 ff',1)]+chunks[1:],
                rows+[chunks[0].replace('ff ff','2f ff',1)]+chunks[1:]):
            with self.subTest(rows=bad),self.assertRaises(observer.ObservationUnavailable):
                observer.directory_queries(bad)

    def test_kernel_return_link_fdinfo_requires_one_exact_function_table_row(self):
        data=(b'pos:\t0\nlink_type:\tkretprobe_multi\nlink_id:\t3\nprog_id:\t6\n'
            b'kprobe_cnt:\t1\nmissed:\t0\ncookie\t func\n0\t getname_flags+0x0/0x20\n')
        self.assertEqual(observer._fdinfo(data)['missed'],'0')
        for bad in (data.replace(b'getname_flags',b'other_function'),data.replace(b'+0x0/',b'+0x1/'),
                data.replace(b'0\t getname',b'1\t getname'),data.replace(b'kprobe_cnt:\t1',b'kprobe_cnt:\t2'),
                data+b'0\t getname_flags+0x0/0x20\n',data.split(b'cookie')[0],data+b'missed:\t0\n'):
            with self.subTest(data=bad),self.assertRaises(observer.ObservationUnavailable):observer._fdinfo(bad)

    def test_only_the_same_sixteen_original_links_and_programs_with_zero_misses_have_a_census(self):
        sample=census_sample();value={'before':sample,'after':copy.deepcopy(sample)}
        observer.validate_census(value)
        for fault in ('missed-before','missed-after','recursion','missing-link','duplicate-program',
                'changed-owner','changed-birth','changed-link','changed-tag','wrong-symbol','multiple-functions',
                'entry-probe','missing-iterator','duplicate-actor','foreign-raw-event','bool-misses','unknown-fields','absent-counter'):
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
            elif fault=='missing-iterator':after['links'][4]['function']='filldir64'
            elif fault=='duplicate-actor':after['links'][5]['function']='filldir64'
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
        for value in (logs,logs*(observer.PROBE_COUNT+3)+data,b'unknown loader output\n'+data,
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
        self.assertEqual(observer.startup_errno(b'BPF program is too large. Processed 1000001 insn\nERROR: Error loading program'),errno.E2BIG)
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
                data.replace(b'101 101',b'0 0'),data.replace(b'69 6e 70 75 74',b'ff'),
                data.replace(b'69 6e 70 75 74',b'69 6e\nforged'),
                data.replace(b'69 6e 70 75 74',b'69 6e 00 75 74'),
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
                [records[0].replace(path[:63].encode().hex(' ').encode(),path[:62].encode().hex(' ').encode())]+records[1:]):
            with self.subTest(rows=rows),self.assertRaises(observer.ObservationUnavailable):observer.decode(count(rows))

    @unittest.skipIf(os.geteuid()==0,'non-root refusal is exercised by a non-root caller')
    def test_no_host_caller_can_start_the_privileged_kernel_probe(self):
        with self.assertRaises(observer.ObservationUnavailable):observer.Observer(None,deadline=0)


if __name__=='__main__':unittest.main()
