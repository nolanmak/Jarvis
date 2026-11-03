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

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('planner_protocol',ROOT/'ci-cargo-planner.py')
planner=importlib.util.module_from_spec(spec);spec.loader.exec_module(planner)


class PlannerProtocolTests(unittest.TestCase):
    def probe(self):
        scopes=[{'scope_id':str(uuid.uuid4())} for _ in range(4)]
        workers={'scopes':scopes};writer={'guest_worker_quiescence':scopes[-1]}
        source={'access':[{'scope':scope} for scope in scopes[:2]]}
        binding={'original':'binding'};command=str(uuid.uuid4())
        cases=[{'uid':2000+i,'stdin_sha256':hashlib.sha256(b'\xffstdin'*200000).hexdigest(),
            'metadata_denied':True,'root_socket_denied':True,'control_fds_closed':True,'jobserver':i==1} for i in range(2)]
        cases.append({'cancelled_before_exec':True,'proxy_exit':125})
        units=[{'ordinal':i,'uid':2000+i,'started':i!=2,'cancelled':i==2,'returncode':0 if i!=2 else -9,
            'original_cleanup':True,'input_closure_complete':False,'shipping_authority':False} for i in range(3)]
        value={'version':3,'kind':'ci-planner-protocol-probe','binding':binding,'command':command,
            'cases':cases,'units':units,'guest_worker_quiescence':scopes[2],
            'input_closure_complete':False,'cargo_metadata_isolation_established':False,'shipping_authority':False}
        return value,binding,command,workers,writer,source

    def test_probe_requires_original_distinct_scope_roles_bindings_and_no_authority(self):
        original,binding,command,workers,writer,source=self.probe()
        self.assertEqual(planner.validate_probe(original,binding,command,workers,writer,source),original)
        for fault in ('version','binding','command','scope','writer','source','role','cancelled-start',
                'cleanup','case-role','control-fds','jobserver','closure','authority','unknown'):
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
            with self.subTest(fault=fault),self.assertRaises(planner.FreshnessHold):
                planner.validate_probe(value,binding,command,workers,writer,source)

    def test_pinned_probe_parent_streams_stdin_and_waits_for_real_exec_child(self):
        # Exercise the fixed fixture's real fork/exec/pipe parent path without
        # claiming the root-only transport or credential isolation on this host.
        # Only the root-only setup assertions and unit policy result are stubs.
        peer = """import hashlib,json,os,re,sys
flags=os.environ.get('CARGO_MAKEFLAGS')
if flags:
 numbers=re.search(r'--jobserver-fds=(\\d+),(\\d+)',flags)
 reader,writer=map(int,numbers.groups())
 assert os.read(reader,1)==b'+'
 os.write(writer,b'+')
assert os.environb[b'RAW']==b'\\xffvalue'
print(json.dumps({'uid':int(sys.argv[1]),'stdin_sha256':hashlib.sha256(sys.stdin.buffer.read()).hexdigest(),
 'metadata_denied':True,'root_socket_denied':True,'control_fds_closed':True,'jobserver':bool(flags)}),flush=True)
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
            for mode in ('stdin','jobserver'):
                with self.subTest(mode=mode):
                    result=subprocess.run([sys.executable,'-I','-c',harness,str(ROOT/'ci-cargo-planner.py'),peer,mode,
                        str(Path(directory)/'metadata')],capture_output=True,timeout=5)
                    self.assertEqual(result.returncode,0,result.stderr.decode(errors='replace'))
                    observed=__import__('json').loads(result.stdout)
                    self.assertEqual(observed['stdin_sha256'],hashlib.sha256(b'\xffstdin'*200000).hexdigest())
                    self.assertEqual(observed['jobserver'],mode=='jobserver')

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
