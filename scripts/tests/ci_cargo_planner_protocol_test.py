"""V3 transport is distinct from the ordinary same-UID relay and grants no root API."""
import fcntl
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import uuid
import copy
import hashlib
import json
import socket
import array
from types import SimpleNamespace
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('planner_protocol',ROOT/'ci-cargo-planner.py')
planner=importlib.util.module_from_spec(spec);spec.loader.exec_module(planner)


class PlannerProtocolTests(unittest.TestCase):
    def test_actual_library_and_build_script_graph_requests_keep_original_paths(self):
        generated=b'/workspace/target/debug/build/private-cargo-probe-bbbbbbbbbbbbbbbb/out'
        def compile(source,name,kind,out,emit,extra=None):
            args=[b'/toolchain/bin/rustc',b'--crate-name',name,b'--edition=2021',source,
                b'--crate-type',kind,emit,b'-C',b'embed-bitcode=no',b'-C',b'metadata=0123456789abcdef',
                b'-C',b'extra-filename=-aaaaaaaaaaaaaaaa',b'--out-dir',out,
                b'-L',b'dependency=/workspace/target/debug/deps',*(extra or [])]
            return args[0],args,{b'OUT_DIR':generated},b'/workspace',b'/toolchain/bin:/usr/bin:/bin',0,0
        build=compile(b'build.rs',b'build_script_build',b'bin',
            b'/workspace/target/debug/build/private-cargo-probe-aaaaaaaaaaaaaaaa',b'--emit=dep-info,link')
        build[2].clear()
        self.assertEqual(planner.fixture_request(build)['role'],'build-script')
        library=compile(b'src/lib.rs',b'private_cargo_probe',b'lib',
            b'/workspace/target/debug/deps',b'--emit=dep-info,metadata,link')
        result=planner.fixture_request(library)
        self.assertEqual(result['role'],'library')
        self.assertEqual(result['generated_root'],'debug/build/private-cargo-probe-bbbbbbbbbbbbbbbb/out')
        self.assertIn('debug/deps/libprivate_cargo_probe-aaaaaaaaaaaaaaaa.rlib',result['outputs'])
        self.assertIn('debug/deps/libprivate_cargo_probe-aaaaaaaaaaaaaaaa.rmeta',result['outputs'])
        binary=compile(b'src/main.rs',b'private_cargo_probe',b'bin',
            b'/workspace/target/debug/deps',b'--emit=dep-info,link',
            [b'--extern',b'private_cargo_probe=/workspace/target/debug/deps/libprivate_cargo_probe-aaaaaaaaaaaaaaaa.rlib'])
        self.assertEqual(planner.fixture_request(binary)['dependencies'],
            ['debug/deps/libprivate_cargo_probe-aaaaaaaaaaaaaaaa.rlib'])
        executable=b'/workspace/target/debug/build/private-cargo-probe-aaaaaaaaaaaaaaaa/build-script-build'
        request=(executable,[executable],{b'OUT_DIR':generated},b'/workspace',None,0,0)
        self.assertEqual(planner.fixture_request(request),{'kind':'build_script',
            'alias':'debug/build/private-cargo-probe-aaaaaaaaaaaaaaaa/build-script-build',
            'generated_root':'debug/build/private-cargo-probe-bbbbbbbbbbbbbbbb/out'})
        for request in (library[:2]+({b'OUT_DIR':b'/etc'},)+library[3:],
                compile(b'build.rs',b'build_script_build',b'lib',b'/workspace/target/debug/deps',b'--emit=dep-info,link')):
            with self.subTest(request=request),self.assertRaises(planner.FreshnessHold):planner.fixture_request(request)

    def test_cargo_run_relative_executable_keeps_its_original_request(self):
        # Cargo 1.94.1 cargo_run.rs strips the caller's cwd from its executable.
        file=b'target/debug/private-cargo-probe'
        request=(file,[file],{},b'/workspace',b'/toolchain/bin:/usr/bin:/bin',0,0)
        self.assertEqual(planner.fixture_request(request),{'kind':'application','alias':'debug/private-cargo-probe'})
        self.assertEqual(request[0],file)
        for unknown in (b'../target/debug/private-cargo-probe',b'other/debug/private-cargo-probe',b'private-cargo-probe'):
            with self.subTest(file=unknown),self.assertRaises(planner.FreshnessHold):
                planner.fixture_request((unknown,[unknown],{},b'/workspace',request[4],0,0))

    def test_rejected_mapping_closes_original_request_control_descriptors(self):
        class Reject:
            def prepare(self,*args):raise planner.FreshnessHold('input_census_unknown')
        with tempfile.TemporaryDirectory() as directory:
            relay=object.__new__(planner.RootRelay);relay.root=Path(directory)
            (relay.root/'units').mkdir();relay.timeout=1;relay.library=Path('/fixed/library')
            relay.admission=SimpleNamespace(target='/fixed/target',binding={},command='fixed')
            relay.views=Reject()
            before=set(os.listdir('/proc/self/fd'))
            with patch.object(planner,'payload',return_value=()),self.assertRaises(planner.FreshnessHold):
                relay._run(None,5,[0],[0,1,2],['planner_unit'])
            self.assertEqual(set(os.listdir('/proc/self/fd')),before)

    def test_fixed_cargo_mapper_refuses_unknown_inputs_outputs_and_commands(self):
        def request(args,file=b'/toolchain/bin/rustc',cwd=b'/workspace',search=None):
            return file,[file,*args],{},cwd,search,0,0
        self.assertEqual(planner.fixture_request(request([b'-vV']))['kind'],'information')
        self.assertEqual(planner.fixture_request(request([b'-',b'--crate-name',b'___',
            b'--print=file-names',b'--crate-type',b'bin']))['kind'],'information')
        args=[b'--crate-name',b'private_cargo_probe',b'--edition=2021',b'src/main.rs',
            b'--crate-type',b'bin',b'--emit=dep-info,link',b'-C',b'embed-bitcode=no',
            b'-C',b'metadata=0123456789abcdef',b'-C',b'extra-filename=-0123456789abcdef',
            b'--out-dir',b'/workspace/target/debug/deps',b'-L',b'dependency=/workspace/target/debug/deps']
        mapped=planner.fixture_request(request(args))
        self.assertEqual(mapped,{'kind':'compile','role':'binary','dependencies':[],'generated_root':None,'outputs':[
            'debug/deps/private_cargo_probe-0123456789abcdef',
            'debug/deps/private_cargo_probe-0123456789abcdef.d']})
        self.assertEqual(planner.fixture_request(request(args,search=b'/unrelated/path')),mapped)
        for extra in ([b'--extern',b'evil=/tmp/evil.rlib'],[b'-o',b'/tmp/escape'],
                [b'@/tmp/response'],[b'--cfg',b'unknown'],[b'other.rs'],
                [b'--out-dir',b'/workspace/target/debug/deps']):
            with self.subTest(extra=extra),self.assertRaises(planner.FreshnessHold):
                planner.fixture_request(request([*args,*extra]))
        for invalid in (request([b'--print=sysroot',b'/tmp/input.rs']),
                request(args,cwd=b'/tmp'),request(args,file=b'rustc',search=b'/toolchain/bin'),
                request([],file=b'/bin/sh'),request([b'--version'],file=b'/tmp/rustc')):
            with self.subTest(invalid=invalid),self.assertRaises(planner.FreshnessHold):
                planner.fixture_request(invalid)
        self.assertEqual(planner.fixture_request(request([],file=b'/workspace/target/debug/private-cargo-probe')),
            {'kind':'application','alias':'debug/private-cargo-probe'})

    def test_cargo_probe_requires_actual_unit_cleanup_and_preserves_no_authority(self):
        units=[{'ordinal':i,'uid':2000+i,'started':True,'cancelled':False,'returncode':0,
            'original_cleanup':True,'input_closure_complete':False,'shipping_authority':False} for i in range(5,14)]
        value={'version':1,'compiler_calls':[3,0],'build_script_calls':[1,0],'generated_tree_preserved':True,'library_input_mounted':True,'application_runs':2,'actual_metadata_preserved':True,
            'sealed_artifact_detached':True,'units':units,'allocation_removed':True,
            'trusted_input_closure':False,'shipping_authority':False}
        self.assertEqual(planner.validate_cargo_probe(value),value)
        for field,replacement in (('compiler_calls',[True,0]),('compiler_calls',[3,1]),
                ('build_script_calls',[True,0]),('build_script_calls',[1,1]),('generated_tree_preserved',False),('library_input_mounted',False),
                ('application_runs',True),('actual_metadata_preserved',False),('sealed_artifact_detached',False),
                ('allocation_removed',False),('trusted_input_closure',True),('shipping_authority',True)):
            with self.subTest(field=field),self.assertRaises(planner.FreshnessHold):
                planner.validate_cargo_probe({**value,field:replacement})
        for field,replacement in (('uid',1001),('ordinal',True),('started',False),('cancelled',True),
                ('returncode',125),('original_cleanup',False),('input_closure_complete',True),('shipping_authority',True)):
            bad=copy.deepcopy(value);bad['units'][0][field]=replacement
            with self.subTest(field=field),self.assertRaises(planner.FreshnessHold):planner.validate_cargo_probe(bad)

    def test_dynamic_output_report_requires_exact_declared_root_inventory(self):
        original=self.probe()[0]['filesystem']
        changed=copy.deepcopy(original)
        changed['outputs'][3][0]=b'unit-output/generated-1234.bin'.hex()
        self.assertEqual(planner.validate_filesystem_probe(changed),changed)
        for fault in ('escape','unknown','malformed','empty-removed','extra','digest','bytes','alias'):
            value=copy.deepcopy(original)
            if fault=='escape':value['outputs'][3][0]=b'../unit-output/generated-42.bin'.hex()
            elif fault=='unknown':value['outputs'][3][0]=b'unit-output/unplanned.bin'.hex()
            elif fault=='malformed':value['outputs'][3][0]='not hex'
            elif fault=='empty-removed':del value['outputs'][2]
            elif fault=='extra':value['outputs'].append(value['outputs'][3])
            elif fault=='digest':value['outputs'][3][4]='0'*64
            elif fault=='bytes':value['outputs'][3][3]=13
            elif fault=='alias':value['outputs'][3][1]='symlink'
            with self.subTest(fault=fault),self.assertRaises(planner.FreshnessHold):
                planner.validate_filesystem_probe(value)

    def probe(self):
        scopes=[{'scope_id':str(uuid.uuid4())} for _ in range(4)]
        workers={'scopes':scopes};writer={'guest_worker_quiescence':scopes[-1]}
        source={'access':[{'scope':scope} for scope in scopes[:2]]}
        binding={'original':'binding'};command=str(uuid.uuid4())
        cases=[{'uid':2000+i,'stdin_sha256':hashlib.sha256(b'\xffstdin'*200000).hexdigest(),
            'metadata_denied':True,'root_socket_denied':True,'control_fds_closed':True,'jobserver':i==1} for i in range(2)]
        cases.append({'cancelled_before_exec':True,'proxy_exit':125})
        cases.append({'uid':2003,'stdin_sha256':hashlib.sha256(b'\xffstdin'*200000).hexdigest(),
            'metadata_denied':True,'root_socket_denied':True,'control_fds_closed':True,'jobserver':False,
            'signal_forwarded':10,'proxy_signal':10})
        filesystem_case={'input_stable':True,'input_metadata_stable':True,'source_read_only':True,
            'abstract_socket_isolated':True,'private_temp':True,'metadata_hidden':True,'output_written':True}
        cases.append({'uid':2004,'stdin_sha256':hashlib.sha256(b'\xffstdin'*200000).hexdigest(),
            'metadata_denied':True,'root_socket_denied':True,'control_fds_closed':True,'jobserver':False,
            'filesystem':filesystem_case})
        filesystem={'version':1,'input_bytes_preserved':True,'input_inode_metadata_preserved':True,
            'next_publication_distinct':True,'namespaces':{name:{'controller':name+':[1]','unit':name+':[2]'}
                for name in ('mnt','ipc','net')},'outputs':[['2e','directory',0o555],
                [b'unit-output'.hex(),'directory',0o555],[b'unit-output/empty'.hex(),'directory',0o555],
                [b'unit-output/generated-42.bin'.hex(),'file',0o444,14,hashlib.sha256(b'sealed result\n').hexdigest()]],
            'original_unit_cleanup':True,'allocation_removed':True,'shipping_authority':False}
        units=[{'ordinal':i,'uid':2000+i,'started':i!=2,'cancelled':i==2,'returncode':(-9 if i==2 else -10 if i==3 else 0),
            'original_cleanup':True,'input_closure_complete':False,'shipping_authority':False} for i in range(5)]
        value={'version':3,'kind':'ci-planner-protocol-probe','binding':binding,'command':command,
            'cases':cases,'units':units,'filesystem':filesystem,'guest_worker_quiescence':scopes[2],
            'input_closure_complete':False,'cargo_metadata_isolation_established':False,'shipping_authority':False}
        value['kernel_observation']={'version':1,'kind':'kernel-copied-filename-probe','binding':binding,'command':command,
            'shipping_authority':False,'kernel_release':'7.0.0-test','unit_pid':42,'unit_uid':2004,
            'program_sha256':'1'*64,'binary_sha256':'2'*64,'witness_observed':True,'positive_query_observed':True,
            'negative_query_observed':True,'max_path_query_observed':True,'compiler_queries_observed':True,'library_generated_query_observed':True,'after_kernel_copy':True,'events':3,'original_observer_cleanup':True,
            'filesystem_coverage_complete':False,'missed_probe_census_complete':True,'input_closure_complete':False,
            'allocation_removed':True}
        value['kernel_observation'].update(directory_reads=8,directory_entries=13,binary_path_query_observed=True,
            directory_witness_observed=True,binary_directory_name_observed=True,
            directory_short_read_observed=True,directory_eof_observed=True,cargo_directory_queries_observed=True)
        census={'pid':100,'birth':1000,
            'programs':[{'id':i,'type':17 if i in (1,4) else 2,'tag':str(i)*16,'recursion_misses':0} for i in range(1,5)],
            'links':[{'id':10+i,'type':'raw_tracepoint' if i in (1,4) else 'kretprobe_multi',
                'prog_id':i,'tag':str(i)*16,**({'tracepoint':'sys_enter' if i==1 else 'sys_exit'} if i in (1,4)
                    else {'function':'getname_flags','count':1,'flags':1,'missed':0})} for i in range(1,5)]}
        for i,(function,flags) in enumerate((('iterate_dir',0),('iterate_dir',1),('filldir64',0),('filldir64',1)),5):
            census['programs'].append({'id':i,'type':2,'tag':str(i)*16,'recursion_misses':0})
            census['links'].append({'id':10+i,'type':'kretprobe_multi' if flags else 'kprobe_multi',
                'prog_id':i,'tag':str(i)*16,'function':function,'count':1,'flags':flags,'missed':0})
        value['kernel_observation']['probe_census']={'before':census,'after':json.loads(json.dumps(census))}
        return value,binding,command,workers,writer,source

    def test_probe_requires_original_distinct_scope_roles_bindings_and_no_authority(self):
        original,binding,command,workers,writer,source=self.probe()
        self.assertEqual(planner.validate_probe(original,binding,command,workers,writer,source),original)
        for fault in ('version','binding','command','scope','writer','source','role','cancelled-start',
                'cleanup','case-role','control-fds','jobserver','closure','authority','unknown',
                'signal-status','signal-proxy','signal-number','signal-not-started','filesystem-namespace',
                'filesystem-output','filesystem-cleanup','filesystem-removed','filesystem-authority','filesystem-case',
                'kernel-binding','kernel-witness','kernel-loss','kernel-coverage','kernel-closure','kernel-cleanup','kernel-missed','kernel-census','kernel-binary-path'):
            value=copy.deepcopy(original)
            if fault=='version':value['version']=True
            elif fault=='binding':value['binding']={}
            elif fault=='command':value['command']=str(uuid.uuid4())
            elif fault=='scope':value['guest_worker_quiescence']={'scope_id':str(uuid.uuid4())}
            elif fault=='writer':value['guest_worker_quiescence']=writer['guest_worker_quiescence']
            elif fault=='source':value['guest_worker_quiescence']=source['access'][0]['scope']
            elif fault=='role':value['units'][0]['uid']=1001
            elif fault=='cancelled-start':value['units'][2]['started']=True
            elif fault=='cleanup':value['units'][0]['original_cleanup']=False
            elif fault=='case-role':value['cases'][0]['uid']=0
            elif fault=='control-fds':value['cases'][0]['control_fds_closed']=False
            elif fault=='jobserver':value['cases'][1]['jobserver']=False
            elif fault=='closure':value['input_closure_complete']=True
            elif fault=='authority':value['shipping_authority']=True
            elif fault=='unknown':value['extra']=True
            elif fault=='signal-status':value['units'][3]['returncode']=0
            elif fault=='signal-proxy':value['cases'][3]['proxy_signal']=15
            elif fault=='signal-number':value['cases'][3]['signal_forwarded']=15
            elif fault=='signal-not-started':value['units'][3]['started']=False
            elif fault=='filesystem-namespace':value['filesystem']['namespaces']['mnt']['unit']='mnt:[1]'
            elif fault=='filesystem-output':value['filesystem']['outputs'][1][-1]='0'*64
            elif fault=='filesystem-cleanup':value['filesystem']['original_unit_cleanup']=False
            elif fault=='filesystem-removed':value['filesystem']['allocation_removed']=False
            elif fault=='filesystem-authority':value['filesystem']['shipping_authority']=True
            elif fault=='filesystem-case':value['cases'][4]['filesystem']['input_stable']=False
            elif fault=='kernel-binding':value['kernel_observation']['binding']={}
            elif fault=='kernel-witness':value['kernel_observation']['witness_observed']=False
            elif fault=='kernel-loss':value['kernel_observation']['events']=True
            elif fault=='kernel-coverage':value['kernel_observation']['filesystem_coverage_complete']=True
            elif fault=='kernel-closure':value['kernel_observation']['input_closure_complete']=True
            elif fault=='kernel-cleanup':value['kernel_observation']['original_observer_cleanup']=False
            elif fault=='kernel-missed':value['kernel_observation']['probe_census']['after']['links'][1]['missed']=1
            elif fault=='kernel-census':del value['kernel_observation']['probe_census']
            elif fault=='kernel-binary-path':value['kernel_observation']['binary_path_query_observed']=False
            with self.subTest(fault=fault),self.assertRaises(planner.FreshnessHold):
                planner.validate_probe(value,binding,command,workers,writer,source)

    def test_pinned_probe_parent_streams_stdin_and_waits_for_real_exec_child(self):
        # Exercise the fixed fixture's real fork/exec/pipe parent path without
        # claiming the root-only transport or credential isolation on this host.
        # Only the root-only setup assertions and unit policy result are stubs.
        peer = """import hashlib,json,os,re,signal,sys
flags=os.environ.get('CARGO_MAKEFLAGS')
if flags:
 numbers=re.search(r'--jobserver-fds=(\\d+),(\\d+)',flags)
 reader,writer=map(int,numbers.groups())
 assert os.read(reader,1)==b'+'
 os.write(writer,b'+')
assert os.environb[b'RAW']==b'\\xffvalue'
print(json.dumps({'uid':int(sys.argv[1]),'stdin_sha256':hashlib.sha256(sys.stdin.buffer.read()).hexdigest(),
 'metadata_denied':True,'root_socket_denied':True,'control_fds_closed':True,'jobserver':bool(flags)}),flush=True)
if os.environ.get('CI_PLANNER_PROBE_SIGNAL')=='1':signal.pause()
"""
        harness = """import ctypes,errno,importlib.util,os,sys
spec=importlib.util.spec_from_file_location('fixture',sys.argv[1]);module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
os.getresuid=lambda:(1001,1001,1001)
os.getresgid=lambda:(1001,1001,1001)
os.getgroups=lambda:[]
class Syscall:
 def __call__(self,*args):ctypes.set_errno(errno.EPERM);return -1
class Libc:syscall=Syscall()
ctypes.CDLL=lambda *args,**kwargs:Libc()
original_exec=os.execve
peer=sys.argv[2];mode=sys.argv[3];metadata=sys.argv[4]
def unit_exec(file,argv,environment):
 return original_exec(sys.executable,[sys.executable,'-I','-c',peer,'2000'],environment)
os.execve=unit_exec
sys.argv=['fixed-fixture',mode,metadata,'/unused-root-socket','2000']
exec(compile(module.guarded_probe(module._PLANNER_PROBE),'fixed-planner-probe','exec'))
"""
        with tempfile.TemporaryDirectory() as directory:
            for mode in ('stdin','jobserver','signal'):
                with self.subTest(mode=mode):
                    result=subprocess.run([sys.executable,'-I','-c',harness,str(ROOT/'ci-cargo-planner.py'),peer,mode,
                        str(Path(directory)/'metadata')],capture_output=True,timeout=5)
                    self.assertEqual(result.returncode,0,result.stderr.decode(errors='replace'))
                    observed=__import__('json').loads(result.stdout)
                    self.assertEqual(observed['stdin_sha256'],hashlib.sha256(b'\xffstdin'*200000).hexdigest())
                    self.assertEqual(observed['jobserver'],mode=='jobserver')
                    if mode=='signal':self.assertEqual(observed['proxy_signal'],10)

    def test_fixed_probe_reports_bounded_phase_without_exception_contents(self):
        private='PRIVATE_OWNER_CONTENTS_AND_PATH'
        body="_PROBE_PHASE='planner_probe_status'\nraise OSError(5,"+repr(private)+")"
        result=subprocess.run([sys.executable,'-I','-c',planner.guarded_probe(body)],capture_output=True,timeout=3)
        self.assertEqual(result.returncode,125)
        self.assertEqual(json.loads(result.stderr),{'planner_diagnostic':{
            'stage':'planner_probe_status','category':'OSError','errno':5}})
        self.assertNotIn(private.encode(),result.stdout+result.stderr)
        for phase in ('planner_probe_credentials','planner_probe_filter','planner_probe_exec_start',
                'planner_probe_stream','planner_probe_wait','planner_probe_status','planner_request_receive',
                'planner_request_cleanup'):
            self.assertEqual(planner.diagnostic({'stage':phase})['stage'],phase)

    def test_worker_failure_is_not_masked_by_proxy_status_or_request_cleanup(self):
        child={'stage':'planner_drop','category':'OSError','errno':13,'private':'PRIVATE_OWNER_CONTENTS'}
        proxy={'stage':'planner_probe_status','category':'Unavailable','errno':None}
        stderr=b'ignored private traceback\n'+json.dumps({'planner_diagnostic':child}).encode()+b'\n'+json.dumps({'planner_diagnostic':proxy}).encode()
        request=[{'stage':'planner_unit','category':'FreshnessHold','errno':None}]
        self.assertEqual(planner.failed_probe_diagnostic(stderr,request),{
            'stage':'planner_drop','category':'OSError','errno':13})
        self.assertEqual(planner.failed_probe_diagnostic(json.dumps({'planner_diagnostic':proxy}).encode(),request),request[0])
        self.assertNotIn('PRIVATE',json.dumps(planner.failed_probe_diagnostic(stderr,request)))

    def test_fixed_tool_refusal_reports_only_credentials_phase(self):
        if os.getresuid()==(2000,2000,2000) and os.getresgid()==(2000,2000,2000):
            self.skipTest('host happens to have the exact admitted unit credentials')
        private='PRIVATE_OWNER_CONTENTS_AND_PATH'
        result=subprocess.run([sys.executable,'-I','-c',planner.guarded_probe(planner._TOOL_PROBE),
            private,private,'2000'],capture_output=True,timeout=3)
        self.assertEqual(result.returncode,125)
        self.assertEqual(json.loads(result.stderr),{'planner_diagnostic':{
            'stage':'planner_tool_credentials','category':'Unavailable','errno':None}})
        self.assertNotIn(private.encode(),result.stdout+result.stderr)
        self.assertEqual(planner.diagnostic({'stage':'planner_unit_exec','category':'TimeoutExpired'})['category'],'TimeoutExpired')

    def test_original_control_socket_eof_with_cloexec_flag_is_normal_completion(self):
        receiver,sender=socket.socketpair(socket.AF_UNIX,socket.SOCK_SEQPACKET|socket.SOCK_CLOEXEC)
        try:
            original=os.pidfd_open(os.getpid())
            try:planner._control(sender,1,os.getpid(),original)
            finally:os.close(original)
            stage,pid,received=planner.receive_control(receiver)
            try:
                self.assertEqual((stage,pid),(1,os.getpid()))
                self.assertEqual(len(received),1)
                self.assertTrue(fcntl.fcntl(received[0],fcntl.F_GETFD)&fcntl.FD_CLOEXEC)
            finally:planner.legacy._close(received)
            planner._control(sender,2);planner._control(sender,3,os.getpid());sender.close()
            self.assertEqual(planner.receive_control(receiver),(2,0,[]))
            self.assertEqual(planner.receive_control(receiver),(3,os.getpid(),[]))
            self.assertIsNone(planner.receive_control(receiver))
        finally:receiver.close();sender.close()

    def test_malformed_control_frames_release_received_capabilities(self):
        original=os.pidfd_open(os.getpid())
        try:
            for frame in (b'',b'short',planner.legacy.RESPONSE.pack(b'JARVCTL3',99,0),
                    planner.legacy.RESPONSE.pack(b'JARVCTL2',1,0),planner.legacy.RESPONSE.pack(b'JARVCTL3',1,0)+b'extra'):
                with self.subTest(frame=frame):
                    receiver,sender=socket.socketpair(socket.AF_UNIX,socket.SOCK_SEQPACKET|socket.SOCK_CLOEXEC)
                    try:
                        sender.sendmsg([frame],[(socket.SOL_SOCKET,socket.SCM_RIGHTS,array.array('i',[original]))])
                        before=set(os.listdir('/proc/self/fd'))
                        with self.assertRaises(ValueError):planner.receive_control(receiver)
                        self.assertEqual(set(os.listdir('/proc/self/fd')),before)
                    finally:receiver.close();sender.close()
        finally:os.close(original)

    def test_staging_and_actual_exec_are_distinct_versioned_events(self):
        self.assertNotEqual(planner.event('staging_ready',0),planner.event('tool_started',0))
        self.assertEqual(planner.decode_event(planner.event('staging_ready',0)),('staging_ready',0))
        self.assertEqual(planner.decode_event(planner.event('tool_started',0)),('tool_started',0))
        with self.assertRaises(ValueError):planner.legacy.decode_response(planner.event('finished',0))
        with self.assertRaises(ValueError):planner.decode_event(planner.legacy.RESPONSE.pack(b'JARVRSP2',2,0))
        for data in (b'',planner.event('finished',0)+b'extra',planner.legacy.RESPONSE.pack(b'JARVRSP3',99,0)):
            with self.subTest(data=data),self.assertRaises(ValueError):planner.decode_event(data)

    def test_only_sealed_v3_payload_bytes_are_accepted(self):
        body=planner.legacy.encode_request(b'/usr/bin/true',[b'true'],[b'RAW=\xff'],b'/workspace')
        for magic,sealed,accepted in ((b'JARVARG3',True,True),(b'JARVARG2',True,False),(b'JARVARG3',False,False)):
            with self.subTest(magic=magic,sealed=sealed):
                fd=os.memfd_create('planner-protocol-test',os.MFD_CLOEXEC|os.MFD_ALLOW_SEALING)
                try:
                    os.write(fd,magic+body[8:])
                    if sealed:fcntl.fcntl(fd,fcntl.F_ADD_SEALS,planner.legacy.SEALS)
                    if accepted:
                        values=planner.payload(fd);self.assertEqual(values[0],b'/usr/bin/true');self.assertEqual(values[2],{b'RAW':b'\xff'})
                    else:
                        with self.assertRaises(ValueError):planner.payload(fd)
                finally:os.close(fd)

    @unittest.skipIf(os.geteuid()==0,'non-root caller refusal requires a non-root test process')
    def test_application_cannot_admit_a_root_listener_or_select_a_worker_role(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(planner.FreshnessHold):planner.admit(Path(directory),{},'candidate',Path(directory))
            with self.assertRaises(planner.FreshnessHold):planner.RootRelay(object(),library='/candidate/program')

    @unittest.skipIf(os.geteuid()==0,'non-root caller refusal requires a non-root test process')
    def test_internal_worker_entry_never_elevates_or_echoes_candidate_control_data(self):
        marker='PRIVATE_UNTRUSTED_BOOTSTRAP'
        for mode in ('--unit','--stage','--probe','--planner'):
            with self.subTest(mode=mode):
                result=subprocess.run([sys.executable,'-I',str(ROOT/'ci-cargo-planner.py'),mode,marker],
                    capture_output=True,timeout=3,env={'PATH':os.defpath,'LD_PRELOAD':'','JARVIS_ROOT_UID':'0'})
                self.assertEqual(result.returncode,125)
                self.assertNotIn(marker.encode(),result.stdout+result.stderr)


if __name__=='__main__':unittest.main()
