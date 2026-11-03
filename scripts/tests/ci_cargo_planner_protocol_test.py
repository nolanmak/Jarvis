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
import threading
import time
from types import SimpleNamespace
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('planner_protocol',ROOT/'ci-cargo-planner.py')
planner=importlib.util.module_from_spec(spec);spec.loader.exec_module(planner)


class PlannerProtocolTests(unittest.TestCase):
    def test_worker_message_credentials_refuse_a_transferred_connected_socket(self):
        for transferred in (False,True):
            with self.subTest(transferred=transferred):
                server,client=socket.socketpair(socket.AF_UNIX,socket.SOCK_SEQPACKET)
                server.setsockopt(socket.SOL_SOCKET,socket.SO_PASSCRED,1)
                relay=object.__new__(planner.WorkerRelay);relay.peer_uid=os.getuid();relay.require_message_credentials=True
                relay.nonce=b'n'*32;descriptors=[];child=None
                body=bytearray(planner.legacy.encode_request(b'/usr/bin/python3',[b'python3'],{},b'/workspace'))
                body[:8]=b'JARVARG3';payload=os.memfd_create('worker-request',os.MFD_CLOEXEC|os.MFD_ALLOW_SEALING)
                os.write(payload,body);fcntl.fcntl(payload,fcntl.F_ADD_SEALS,planner.legacy.SEALS)
                stdin=os.open('/dev/null',os.O_RDONLY);out_r,out_w=os.pipe();err_r,err_w=os.pipe()
                frame=planner.legacy.REQUEST.pack(b'JARVREQ3',relay.nonce,3,0,1,2,*([0]*61))
                rights=array.array('i',[payload,stdin,out_w,err_w])
                try:
                    if transferred:
                        child=os.fork()
                        if child==0:
                            try:client.sendmsg([frame],[(socket.SOL_SOCKET,socket.SCM_RIGHTS,rights)])
                            finally:os._exit(0)
                    else:client.sendmsg([frame],[(socket.SOL_SOCKET,socket.SCM_RIGHTS,rights)])
                    if transferred:
                        with self.assertRaises(planner.FreshnessHold):relay._receive(server,descriptors)
                    else:self.assertEqual(relay._receive(server,descriptors),[0,1,2])
                finally:
                    if child is not None:os.waitpid(child,0)
                    planner.legacy._close([payload,stdin,out_r,out_w,err_r,err_w,*descriptors]);server.close();client.close()

    def test_worker_runtime_holds_opaque_static_and_loader_redirects(self):
        request=(b'/usr/bin/python3',[b'python3',b'-I',b'driver.py'],{},b'/workspace',None,0,0)
        with patch.object(planner.os,'statvfs',return_value=SimpleNamespace(f_flag=os.ST_RDONLY)):
            self.assertTrue(planner.worker_runtime_request(request,'/ci-tools/ci-cargo-planner.so'))
        with patch.object(planner.os,'statvfs',return_value=SimpleNamespace(f_flag=0)),self.assertRaises(planner.FreshnessHold):
            planner.worker_runtime_request(request,'/ci-tools/ci-cargo-planner.so')
        for file,environment in ((b'/bin/true',{}),(b'/workspace/opaque',{}),
                (request[0],{b'LD_LIBRARY_PATH':b'/workspace'}),(request[0],{b'LD_PRELOAD':b'/workspace/evil.so'}),
                (request[0],{b'LD_AUDIT':b'/workspace/evil.so'})):
            with self.subTest(file=file,environment=environment),self.assertRaises(planner.FreshnessHold):
                planner.worker_runtime_request((file,[file],environment,request[3],None,0,0),'/ci-tools/ci-cargo-planner.so')

    def test_worker_capability_cannot_select_planner_credentials_or_an_unpinned_cargo(self):
        base=(b'cargo',[b'cargo',b'test'],{b'PATH':b'/toolchain/bin:/usr/bin:/bin'},b'/workspace',b'/toolchain/bin:/usr/bin:/bin',0,0)
        self.assertTrue(planner.worker_cargo_request(base))
        absolute=(b'/toolchain/bin/cargo',[b'/toolchain/bin/cargo',b'build'],base[2],base[3],None,0,0)
        self.assertTrue(planner.worker_cargo_request(absolute))
        self.assertTrue(planner.worker_cargo_request((absolute[0],[b'cargo',b'build'],*absolute[2:])))
        self.assertFalse(planner.worker_cargo_request((b'/bin/bash',[b'/bin/bash',b'-c',b'cargo test'],{},base[3],None,0,0)))
        for path in (b'/tmp:/toolchain/bin',b'/usr/bin:/toolchain/bin',b':/toolchain/bin'):
            request=(base[0],base[1],{b'PATH':path},base[3],path,0,0)
            with self.subTest(path=path),self.assertRaises(planner.FreshnessHold):planner.worker_cargo_request(request)
        with self.assertRaises(planner.FreshnessHold):planner.execution_identity({'ordinal':0,'execution_role':'worker','execution_uid':1001})
        with self.assertRaises(planner.FreshnessHold):planner.execution_identity({'ordinal':0,'execution_role':'cargo','execution_uid':1000})
        self.assertEqual(planner.execution_identity({'ordinal':7}),2007)
        self.assertEqual(planner.execution_identity({'ordinal':7,'execution_role':'worker','execution_uid':1000}),1000)
        self.assertEqual(planner.execution_identity({'ordinal':7,'execution_role':'cargo','execution_uid':1001}),1001)

    def test_dispatch_admits_pinned_rustc_path_search_without_rewriting_actual_request(self):
        path=b'/toolchain/bin:/usr/bin:/bin'
        request=(b'rustc',[b'rustc',b'-vV'],{b'PATH':path},b'/workspace',path,0,0)
        original=copy.deepcopy(request)
        self.assertEqual(planner.dispatch_request(request,graph=None,planner_target=None,publications={},files={}),{'kind':'information'})
        self.assertEqual(request,original)
        for search in (None,b'/tmp:/toolchain/bin',b':/toolchain/bin',b'/usr/bin:/toolchain/bin',b'./bin:/toolchain/bin'):
            with self.subTest(search=search),self.assertRaises(planner.FreshnessHold):
                planner.dispatch_request((request[0],request[1],request[2],request[3],search,0,0),
                    graph=None,planner_target=None,publications={},files={})
        with self.assertRaises(planner.FreshnessHold):
            planner.dispatch_request((request[0],request[1],{b'PATH':b'/tmp'},request[3],path,0,0),
                graph=None,planner_target=None,publications={},files={})

    def test_original_invocation_preserves_flags_environment_and_rejects_transport_injection(self):
        environment={'CARGO_TARGET_DIR':'/workspace/target','CARGO_HOME':'/ci-cargo-home',
            'RUSTFLAGS':'--cfg original_flag','CUSTOM':'value with spaces\nand newline'}
        argv=['/toolchain/bin/cargo','test','-p','consumer','--no-fail-fast','--','--test-threads=1','module::test']
        captured=planner.validate_invocation(argv,environment,'/workspace')
        self.assertEqual(captured,{'argv':argv,'environment':environment,'cwd':'/workspace'})
        environment['CUSTOM']='changed';argv.append('changed')
        self.assertNotEqual(captured['environment'],environment);self.assertNotEqual(captured['argv'],argv)
        for fault in ({'LD_PRELOAD':'candidate.so'},{'JARVIS_CARGO_RELAY_FD':'3'},
            {'CARGO_TARGET_DIR':'/tmp/target'},{'CARGO_HOME':'/cargo'},{'BAD=KEY':'value'}):
            with self.subTest(fault=fault),self.assertRaises(planner.FreshnessHold):
                planner.validate_invocation(argv,environment|fault,'/workspace')
        with self.assertRaises(planner.FreshnessHold):planner.validate_invocation(['/bin/sh','-c','true'],environment,'/workspace')

    def test_original_stream_capture_is_bounded_before_eof_and_preserves_failure_output(self):
        child=subprocess.Popen([sys.executable,'-c',"import sys;print('original-out');print('original-err',file=sys.stderr);sys.exit(17)"],
            stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        output,error=planner.capture_command(child,time.monotonic()+5)
        self.assertEqual((child.returncode,output,error),(17,b'original-out\n',b'original-err\n'))
        child=subprocess.Popen([sys.executable,'-c',"import os,time;os.write(1,b'x'*2048);time.sleep(10)"],
            stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        try:
            with self.assertRaises(planner.FreshnessHold):planner.capture_command(child,time.monotonic()+5,limit=1024)
            self.assertIsNone(child.poll())
        finally:child.kill();child.wait();child.stdout.close();child.stderr.close()

    def test_restart_gate_keeps_original_grant_and_four_pass_limit_across_reopen(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);binding=self.publication_binding();context={'binding':binding,'command':'original','deadline':time.monotonic()+60}
            scope=root/'command-0';scope.mkdir(mode=0o700)
            planner.cache._write_record(scope/'control.json',{'binding':binding,'command':'original','operation':'run'})
            (scope/'cleanup').write_bytes(b'all-descendants-reaped\n');(scope/'cleanup').chmod(0o600)
            gate=planner.RestartGate(root/'restart.json',context)
            for _ in range(4):gate.begin(context);gate.finish(context,scope/'cleanup')
            self.assertEqual(gate.state()['passes'],4)
            with self.assertRaises(planner.FreshnessHold):gate.begin(context)
            reopened=planner.RestartGate(root/'restart.json',context)
            with self.assertRaises(planner.FreshnessHold):reopened.begin(context)
            self.assertFalse(reopened.state()['shipping_authority'])

    def test_restart_gate_requires_original_cleanup_and_rejects_deadline_extension(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);context={'binding':self.publication_binding(),'command':'original','deadline':time.monotonic()+60}
            gate=planner.RestartGate(root/'restart.json',context);gate.begin(context)
            with self.assertRaises(planner.FreshnessHold):gate.finish(context,root/'missing/cleanup')
            with self.assertRaises(planner.FreshnessHold):gate.begin(context)
            with self.assertRaises(planner.FreshnessHold):planner.RestartGate(root/'restart.json',context|{'deadline':context['deadline']+60})
            self.assertEqual(gate.state()['passes'],1)
            expired_context=context|{'deadline':time.monotonic()-1}
            expired=planner.RestartGate(root/'expired.json',expired_context)
            with self.assertRaises(planner.FreshnessHold):expired.begin(expired_context)

    def publication_binding(self):
        return {'generation':'12345678-1234-4234-8234-123456789abc','head':'a'*40,'tree':'b'*40,
            **{key:'c'*64 for key in ('contract_sha256','authority_sha256','cache_identity_sha256','source_inputs_sha256')}}

    def stored_publication(self,store,*,value=b'old',extra=False):
        identity,root=store.allocate();(root/'debug/build/pkg-aaaaaaaaaaaaaaaa/out').mkdir(parents=True)
        output=root/'debug/build/pkg-aaaaaaaaaaaaaaaa/out';(output/'value.rs').write_bytes(value)
        if extra:(output/'chosen.rs').write_bytes(b'new')
        for path in root.rglob('*'):path.chmod(0o555 if path.is_dir() else 0o444)
        root.chmod(0o555);rows=planner.unit_views.inventory(root,owner=os.geteuid(),sealed=True)
        publication={'root':root,'manifest':rows,'files':[os.fsdecode(bytes.fromhex(row[0])) for row in rows if row[1]=='file'],
            'roots':['debug/build/pkg-aaaaaaaaaaaaaaaa/out'],'action':'generated',
            'cargo_target':{'package_id':'pkg','target_name':'build-script-build','target_kind':['custom-build'],
                'manifest_path':'/workspace/Cargo.toml','source_path':'/workspace/build.rs'},
            'generated_root':'debug/build/pkg-aaaaaaaaaaaaaaaa/out'}
        return identity,publication

    def test_publication_store_preserves_history_and_invalidates_before_namespace_advance(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);(root/'units').mkdir(mode=0o700)
            store=planner.PublicationStore(root/'store',binding=self.publication_binding(),command='original',
                unit_root=root/'units',allow_diagnostic=True)
            first,old=self.stored_publication(store);store.publish(first,old)
            captured=[]
            def changed(publication):
                state=store.state();self.assertEqual(state['namespaces'][old['roots'][0]],first)
                self.assertEqual(state['transition']['producer'],second);captured.append('library');return ['library','binary']
            store.watch(old['roots'][0],changed)
            second,new=self.stored_publication(store,extra=True);store.publish(second,new)
            active,files=store.snapshot();history,_=store.snapshot(historical=True)
            self.assertEqual(store.state()['namespaces'][old['roots'][0]],second)
            self.assertEqual(set(history),{first,second});self.assertEqual(set(active),{second})
            self.assertEqual((old['root']/old['roots'][0]/'value.rs').read_bytes(),b'old')
            self.assertEqual(captured,['library']);self.assertEqual(store.state()['last_affected'],['binary','library'])
            self.assertEqual(files[old['roots'][0]+'/chosen.rs'][0],second)

    def test_publication_store_leaves_interrupted_transition_unusable_and_keeps_old_records(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);(root/'units').mkdir(mode=0o700)
            args={'binding':self.publication_binding(),'command':'original','unit_root':root/'units','allow_diagnostic':True}
            store=planner.PublicationStore(root/'store',**args);first,old=self.stored_publication(store);store.publish(first,old)
            def interrupted(_):raise planner.FreshnessHold('metadata_torn')
            store.watch(old['roots'][0],interrupted);second,new=self.stored_publication(store,extra=True)
            with self.assertRaises(planner.FreshnessHold):store.publish(second,new)
            self.assertEqual(store.state()['namespaces'][old['roots'][0]],first)
            self.assertEqual(store.state()['transition']['producer'],second)
            with self.assertRaises(planner.FreshnessHold):store.snapshot()
            with self.assertRaises(planner.FreshnessHold):planner.PublicationStore(root/'store',**args)
            self.assertTrue((root/'store/records'/first).with_suffix('.json').is_file())
            self.assertTrue(old['root'].is_dir());self.assertTrue(new['root'].is_dir())

    def test_publication_store_requires_bound_successful_original_unit_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);units=root/'units';units.mkdir(mode=0o700);binding=self.publication_binding()
            store=planner.PublicationStore(root/'store',binding=binding,command='original',unit_root=units)
            identity,publication=self.stored_publication(store)
            with self.assertRaises(planner.FreshnessHold):store.publish(identity,publication)
            scope=units/str(uuid.uuid4());scope.mkdir(mode=0o700)
            (scope/'cleanup').write_bytes(b'all-descendants-reaped\n');(scope/'cleanup').chmod(0o600)
            record={'version':3,'binding':binding,'command':'original','ordinal':7,'returncode':1,'exec_observed':True}
            planner.cache._write_record(scope/'exit-status.json',record)
            with self.assertRaises(planner.FreshnessHold):store.publish(identity,publication,scope=scope,ordinal=7)
            planner.cache._write_record(scope/'exit-status.json',record|{'returncode':0})
            store.publish(identity,publication,scope=scope,ordinal=7)
            saved=planner.cache._read_record(root/'store/records'/(identity+'.json'))
            self.assertEqual(saved['evidence']['kind'],'unit');self.assertTrue(saved['evidence']['original_cleanup'])
            self.assertEqual(len(store.state()['history']),1)

    def test_dispatch_uses_graph_and_owned_executable_instead_of_package_labels(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source=root/'source';source.mkdir();source,metadata=self.frozen_graph(source)
            (source/'member/src').chmod(0o755);(source/'member/src/tool.rs').write_bytes(b'fn main(){}')
            (source/'member/src/tool.rs').chmod(0o444);(source/'member/src').chmod(0o555)
            metadata['packages'][0]['targets'].append({'kind':['bin'],'crate_types':['bin'],'name':'tool',
                'src_path':'/workspace/member/src/tool.rs','edition':'2021'})
            graph=planner.CargoGraph(source,json.dumps(metadata).encode(),owner=os.geteuid())
            compiled=self.mapped_request(source=b'src/tool.rs',name=b'tool',kind=b'bin',emit=b'dep-info,link')
            compiled[2].update({b'CARGO_MANIFEST_DIR':b'/workspace/member',b'CARGO_PKG_NAME':b'consumer-package',b'CARGO_PKG_VERSION':b'0.1.0'})
            target=root/'target';(target/'debug/deps').mkdir(parents=True)
            executable=target/'debug/deps/tool-aaaaaaaaaaaaaaaa';executable.write_bytes(b'owned executable');executable.chmod(0o755)
            alias=target/'debug/tool';os.link(executable,alias)
            row=[b'debug/deps/tool-aaaaaaaaaaaaaaaa'.hex(),'file',0o555,16,hashlib.sha256(b'owned executable').hexdigest()]
            admission=graph.admit(compiled,host_triple=b'x86_64-unknown-linux-gnu')['cargo_target']
            publications={'opaque-producer-key':{'action':'binary','cargo_target':admission,
                'files':['debug/deps/tool-aaaaaaaaaaaaaaaa'],'generated_root':None}}
            files={'debug/deps/tool-aaaaaaaaaaaaaaaa':('opaque-producer-key',row)}
            request=(b'target/debug/tool',[b'target/debug/tool',b'--original-argument'],{},b'/workspace',None,0,0)
            mapped=planner.dispatch_request(request,graph=graph,planner_target=target,publications=publications,files=files)
            self.assertEqual(mapped['kind'],'application');self.assertEqual(mapped['artifact'],'debug/deps/tool-aaaaaaaaaaaaaaaa')
            self.assertEqual(mapped['alias'],'debug/tool');self.assertEqual(mapped['cargo_target'],admission)
            publications['opaque-producer-key']['action']='build-script'
            request[2][b'OUT_DIR']=b'/workspace/target/debug/build/consumer-package-bbbbbbbbbbbbbbbb/out'
            with self.assertRaises(planner.FreshnessHold):
                planner.dispatch_request(request,graph=graph,planner_target=target,publications=publications,files=files)
            publications['opaque-producer-key']['action']='binary';request[2].clear()
            alias.unlink();alias.write_bytes(b'foreign executable')
            with self.assertRaises(planner.FreshnessHold):
                planner.dispatch_request(request,graph=graph,planner_target=target,publications=publications,files=files)

    def test_dispatch_requires_declared_compiler_target_and_owned_generated_namespace(self):
        with tempfile.TemporaryDirectory() as directory:
            root,metadata=self.frozen_graph(directory)
            (root/'member').chmod(0o755);(root/'member/build.rs').write_bytes(b'fn main(){}')
            (root/'member/build.rs').chmod(0o444);(root/'member').chmod(0o555)
            metadata['packages'][0]['targets'].append({'kind':['custom-build'],'crate_types':['bin'],
                'name':'build-script-build','src_path':'/workspace/member/build.rs','edition':'2021'})
            graph=planner.CargoGraph(root,json.dumps(metadata).encode(),owner=os.geteuid())
            build=self.mapped_request(source=b'build.rs',name=b'build_script_build',kind=b'bin',emit=b'dep-info,link',
                out=b'/workspace/target/debug/build/consumer-package-0123456789abcdef')
            build[2].update({b'CARGO_MANIFEST_DIR':b'/workspace/member',b'CARGO_PKG_NAME':b'consumer-package',b'CARGO_PKG_VERSION':b'0.1.0'})
            producer=graph.admit(build,host_triple=b'x86_64-unknown-linux-gnu')['cargo_target']
            request=self.graph_request()
            mapped=planner.dispatch_request(request,graph=graph,planner_target=root,publications={},files={})
            self.assertEqual(mapped['role'],'library');self.assertEqual(mapped['cargo_target']['target_name'],'consumer')
            request[2][b'OUT_DIR']=b'/workspace/target/debug/build/consumer-package-aaaaaaaaaaaaaaaa/out'
            with self.assertRaises(planner.FreshnessHold):
                planner.dispatch_request(request,graph=graph,planner_target=root,publications={},files={})
            owned={'arbitrary-label':{'action':'generated','roots':['debug/build/consumer-package-aaaaaaaaaaaaaaaa/out'],
                'cargo_target':producer}}
            mapped=planner.dispatch_request(request,graph=graph,planner_target=root,publications=owned,files={})
            self.assertEqual(mapped['generated_root'],'debug/build/consumer-package-aaaaaaaaaaaaaaaa/out')
            owned['arbitrary-label']['cargo_target']['package_id']='foreign-package'
            with self.assertRaises(planner.FreshnessHold):
                planner.dispatch_request(request,graph=graph,planner_target=root,publications=owned,files={})

    def frozen_graph(self,directory):
        root=Path(directory);member=root/'member';(member/'src').mkdir(parents=True)
        (root/'Cargo.lock').write_text('version=4\n')
        (member/'Cargo.toml').write_text('[package]\nname="consumer-package"\nversion="0.1.0"\nedition="2021"\n[lib]\nname="consumer"\n')
        (member/'src/lib.rs').write_bytes(b'pub fn value()->u8{7}\n')
        for path in root.rglob('*'):path.chmod(0o555 if path.is_dir() else 0o444)
        root.chmod(0o555)
        target={'kind':['lib'],'crate_types':['lib'],'name':'consumer','src_path':'/workspace/member/src/lib.rs','edition':'2021'}
        package={'id':'path+file:///workspace/member#consumer-package@0.1.0','name':'consumer-package','version':'0.1.0',
            'manifest_path':'/workspace/member/Cargo.toml','source':None,'targets':[target]}
        metadata={'version':1,'workspace_root':'/workspace','target_directory':'/workspace/target',
            'packages':[package],'workspace_members':[package['id']]}
        return root,metadata

    def graph_request(self):
        request=self.mapped_request();request[2].update({b'CARGO_MANIFEST_DIR':b'/workspace/member',
            b'CARGO_PKG_NAME':b'consumer-package',b'CARGO_PKG_VERSION':b'0.1.0',b'CARGO_CRATE_NAME':b'consumer'})
        return request

    def test_cargo_graph_binds_actual_manifest_target_source_and_package_environment(self):
        with tempfile.TemporaryDirectory() as directory:
            root,metadata=self.frozen_graph(directory)
            graph=planner.CargoGraph(root,json.dumps(metadata).encode(),owner=os.geteuid())
            admitted=graph.admit(self.graph_request(),host_triple=b'x86_64-unknown-linux-gnu')
            self.assertEqual(admitted['cargo_target']['package_name'],'consumer-package')
            self.assertEqual(admitted['cargo_target']['target_name'],'consumer')
            self.assertEqual(admitted['cargo_target']['manifest_path'],'/workspace/member/Cargo.toml')
            self.assertFalse(admitted['shipping_authority'])
            for key,value in ((b'CARGO_PKG_NAME',b'other'),(b'CARGO_PKG_VERSION',b'9.0.0'),
                    (b'CARGO_MANIFEST_DIR',b'/workspace/other'),(b'CARGO_CRATE_NAME',b'other')):
                request=self.graph_request();request[2][key]=value
                with self.subTest(key=key),self.assertRaises(planner.FreshnessHold):
                    graph.admit(request,host_triple=b'x86_64-unknown-linux-gnu')
            request=self.graph_request();request[1][request[1].index(b'src/lib.rs')]=b'Cargo.toml'
            with self.assertRaises(planner.FreshnessHold):graph.admit(request,host_triple=b'x86_64-unknown-linux-gnu')

    def test_cargo_graph_refuses_ambiguous_or_foreign_metadata(self):
        with tempfile.TemporaryDirectory() as directory:
            root,original=self.frozen_graph(directory)
            for fault in ('duplicate-package','duplicate-target','escape','foreign-root','missing-member'):
                metadata=copy.deepcopy(original)
                if fault=='duplicate-package':metadata['packages']*=2
                elif fault=='duplicate-target':metadata['packages'][0]['targets']*=2
                elif fault=='escape':metadata['packages'][0]['targets'][0]['src_path']='/workspace/member/../../outside.rs'
                elif fault=='foreign-root':metadata['workspace_root']='/tmp'
                else:metadata['workspace_members']=[]
                with self.subTest(fault=fault),self.assertRaises(planner.FreshnessHold):
                    planner.CargoGraph(root,json.dumps(metadata).encode(),owner=os.geteuid())

    def test_cargo_graph_refuses_changed_frozen_source_even_with_old_timestamp(self):
        with tempfile.TemporaryDirectory() as directory:
            root,metadata=self.frozen_graph(directory)
            graph=planner.CargoGraph(root,json.dumps(metadata).encode(),owner=os.geteuid())
            source=root/'member/src/lib.rs';original=source.stat();source.chmod(0o600);source.write_bytes(b'pub fn value()->u8{9}\n')
            source.chmod(0o444);os.utime(source,ns=(original.st_atime_ns,original.st_mtime_ns))
            with self.assertRaises(planner.FreshnessHold):graph.admit(self.graph_request(),host_triple=b'x86_64-unknown-linux-gnu')
        with tempfile.TemporaryDirectory() as directory:
            root,metadata=self.frozen_graph(directory)
            graph=planner.CargoGraph(root,json.dumps(metadata).encode(),owner=os.geteuid())
            (root/'member/src').chmod(0o777)
            with self.assertRaises(planner.FreshnessHold):graph.admit(self.graph_request(),host_triple=b'x86_64-unknown-linux-gnu')

    def mapped_request(self,*,source=b'src/lib.rs',name=b'consumer',kind=b'lib',emit=b'dep-info,metadata,link',extra=(),cwd=b'/workspace/member',out=b'/workspace/target/debug/deps'):
        argv=[b'/toolchain/bin/rustc',b'--crate-name',name,b'--edition=2021',source,
            *([b'--crate-type',kind] if kind is not None else []),b'--emit='+emit,
            b'-C',b'metadata=0123456789abcdef',b'-C',b'extra-filename=-0123456789abcdef',
            b'--out-dir',out,b'-L',b'dependency=/workspace/target/debug/deps',*extra]
        return argv[0],argv,{},cwd,None,0,0

    def test_shared_rustc_mapping_keeps_workspace_sources_cfgs_and_multiple_dependencies(self):
        extra=[b'--cfg',b'feature="one"',b'--cfg',b'custom_cfg',b'--check-cfg',b'cfg(feature, values("one"))',
            b'--extern',b'first=/workspace/target/debug/deps/libfirst-aaaaaaaaaaaaaaaa.rlib',
            b'--extern',b'second=/workspace/target/debug/deps/libsecond-bbbbbbbbbbbbbbbb.rmeta']
        request=self.mapped_request(extra=extra)
        mapped=planner.rustc_unit(request,package=b'consumer-package',host_triple=b'x86_64-unknown-linux-gnu')
        self.assertEqual(mapped['source'],b'/workspace/member/src/lib.rs')
        self.assertEqual(mapped['action'],'library')
        self.assertEqual(mapped['cfgs'],[b'feature="one"',b'custom_cfg'])
        self.assertEqual(mapped['dependencies'],['debug/deps/libfirst-aaaaaaaaaaaaaaaa.rlib','debug/deps/libsecond-bbbbbbbbbbbbbbbb.rmeta'])
        self.assertEqual(mapped['outputs'],['debug/deps/libconsumer-0123456789abcdef.rlib',
            'debug/deps/libconsumer-0123456789abcdef.rmeta','debug/deps/consumer-0123456789abcdef.d'])
        self.assertEqual(mapped['fingerprint'],'debug/.fingerprint/consumer-package-0123456789abcdef')
        changed=planner.rustc_unit(self.mapped_request(extra=extra+[b'--cfg',b'feature="two"']),
            package=b'consumer-package',host_triple=b'x86_64-unknown-linux-gnu')
        self.assertNotEqual(mapped['request_sha256'],changed['request_sha256'])
        self.assertFalse(mapped['input_closure_complete']);self.assertFalse(mapped['shipping_authority'])

    def test_shared_rustc_mapping_distinguishes_check_tests_profiles_and_target_paths(self):
        cases=[(self.mapped_request(emit=b'dep-info,metadata'),'check',['debug/deps/libconsumer-0123456789abcdef.rmeta','debug/deps/consumer-0123456789abcdef.d']),
            (self.mapped_request(kind=None,emit=b'dep-info,link',extra=[b'--test']),'test',
                ['debug/deps/consumer-0123456789abcdef','debug/deps/consumer-0123456789abcdef.d']),
            (self.mapped_request(source=b'bin/tool.rs',name=b'tool',kind=b'bin',emit=b'dep-info,link',
                out=b'/workspace/target/aarch64-unknown-linux-gnu/release/deps',extra=[b'--target',b'aarch64-unknown-linux-gnu',b'-C',b'opt-level=3']),
                'binary',['aarch64-unknown-linux-gnu/release/deps/tool-0123456789abcdef','aarch64-unknown-linux-gnu/release/deps/tool-0123456789abcdef.d'])]
        for request,action,outputs in cases:
            with self.subTest(action=action):
                mapped=planner.rustc_unit(request,package=b'consumer-package',host_triple=b'x86_64-unknown-linux-gnu')
                self.assertEqual(mapped['action'],action);self.assertEqual(mapped['outputs'],outputs)

    def test_shared_rustc_mapping_refuses_escaping_or_unmodeled_requests(self):
        for request in (self.mapped_request(source=b'../../outside.rs'),self.mapped_request(out=b'/tmp/out'),
                self.mapped_request(extra=[b'-o',b'/tmp/out']),self.mapped_request(extra=[b'@/tmp/args']),
                self.mapped_request(extra=[b'-C',b'incremental=/tmp/cache']),self.mapped_request(kind=b'proc-macro'),
                self.mapped_request(extra=[b'--extern',b'evil=/tmp/evil.rlib']),
                self.mapped_request(extra=[b'--crate-name',b'duplicate'])):
            with self.subTest(request=request),self.assertRaises(planner.FreshnessHold):
                planner.rustc_unit(request,package=b'consumer-package',host_triple=b'x86_64-unknown-linux-gnu')

    def test_planner_drain_requires_joined_threads_and_no_original_children(self):
        relay=object.__new__(planner.RootRelay)
        relay._threads=[];relay._children=set();relay._lock=threading.Lock();relay.errors=[]
        relay.drain()
        relay._children.add(object())
        with self.assertRaises(planner.FreshnessHold):relay.drain()
        relay._children.clear();relay.errors.append({'stage':'planner_unit'})
        with self.assertRaises(planner.FreshnessHold):relay.drain()
        relay.errors.clear()
        class StillRunning:
            def join(self,timeout):pass
            def is_alive(self):return True
        relay._threads=[StillRunning()]
        with self.assertRaises(planner.FreshnessHold):relay.drain()

    def test_original_cargo_cleanup_refuses_missing_torn_public_and_linked_receipts(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);path=root/'cleanup';body=b'all-descendants-reaped\n'
            with self.assertRaises(planner.FreshnessHold):planner.original_cargo_cleanup(path,owner=os.geteuid())
            path.write_bytes(body);path.chmod(0o600)
            self.assertIsNotNone(planner.original_cargo_cleanup(path,owner=os.geteuid()))
            for fault in ('torn','public','hardlink','symlink'):
                path.write_bytes(body);path.chmod(0o600)
                if fault=='torn':path.write_bytes(b'parent exited\n')
                elif fault=='public':path.chmod(0o644)
                elif fault=='hardlink':os.link(path,root/'alias')
                else:
                    path.rename(root/'backing');path.symlink_to(root/'backing')
                with self.subTest(fault=fault),self.assertRaises(planner.FreshnessHold):
                    planner.original_cargo_cleanup(path,owner=os.geteuid())
                if fault=='hardlink':(root/'alias').unlink()
                if fault=='symlink':path.unlink()

    def test_fingerprint_retirement_preserves_unrelated_units_and_refuses_unknown_work(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);metadata=root/'debug/.fingerprint';metadata.mkdir(parents=True)
            for name in ('library-abcd','binary-abcd','unrelated-abcd'):
                (metadata/name).mkdir();(metadata/name/'record').write_bytes(name.encode())
            expected=planner.unit_views.inventory(metadata,owner=os.geteuid(),sealed=False)
            for paths in (['debug/.fingerprint/missing'],['debug/.fingerprint/../source'],['debug/.fingerprint/library-abcd']*2):
                with self.subTest(paths=paths),self.assertRaises(planner.FreshnessHold):
                    planner.retire_unit_fingerprints(root,expected,paths,owner=os.geteuid())
                self.assertEqual(planner.unit_views.inventory(metadata,owner=os.geteuid(),sealed=False),expected)
            (metadata/'library-abcd/record').write_bytes(b'changed')
            with self.assertRaises(planner.FreshnessHold):
                planner.retire_unit_fingerprints(root,expected,['debug/.fingerprint/library-abcd'],owner=os.geteuid())
            (metadata/'library-abcd/record').write_bytes(b'library-abcd')
            result=planner.retire_unit_fingerprints(root,expected,
                ['debug/.fingerprint/library-abcd','debug/.fingerprint/binary-abcd'],owner=os.geteuid())
            self.assertEqual((metadata/'unrelated-abcd/record').read_bytes(),b'unrelated-abcd')
            self.assertFalse((metadata/'library-abcd').exists());self.assertFalse((metadata/'binary-abcd').exists())
            self.assertEqual(result,planner.unit_views.inventory(metadata,owner=os.geteuid(),sealed=False))

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
            (relay.root/'units').mkdir();relay.timeout=1;relay.deadline=None;relay.library=Path('/fixed/library')
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
        self.assertEqual(mapped,{'kind':'compile','role':'binary','dependencies':[],'generated_root':None,
            'fingerprint':'debug/.fingerprint/private-cargo-probe-0123456789abcdef','outputs':[
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
            'trusted_input_closure':False,'shipping_authority':False,'invalidation':self.compiler_invalidation(),
            'retry':self.cargo_retry(),'cargo_graph':self.cargo_graph(),'publication_store':self.publication_store()}
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

    def compiler_invalidation(self):
        return {'version':1,'unit_pid':42,'unit_uid':2008,'queries':1,'binding_sha256':'1'*64,
            'original_sha256':'2'*64,'refreshed_sha256':'3'*64,'pending':['binary','library'],
            'durable':True,'original_generated_preserved':True,'input_closure_complete':False,'shipping_authority':False}

    def cargo_retry(self):
        return {'version':1,'cargo_exit':101,'library_exit':1,'compiler_calls':1,'diagnostic':'E0761',
            'rounds':1,'original_planner_cleanup':True,'original_unit_cleanup':True,
            'unaffected_fingerprints_preserved':True,'original_generated_preserved':True,
            'failed_outputs_unpublished':True,'pending':['binary','library'],
            'planner_passes':2,'planner_pass_limit':4,'original_grant_unchanged':True,
            'units':[{'ordinal':14,'uid':2014,'started':True,'cancelled':False,'returncode':1,
                'original_cleanup':True,'input_closure_complete':False,'shipping_authority':False}],
            'input_closure_complete':False,'shipping_authority':False}

    def publication_store(self):
        return {'version':1,'history_records':5,'active_producers':4,'unit_producers':4,'diagnostic_producers':1,
            'invalidation_before_activation':True,'last_affected':['binary','library'],'original_unit_cleanup':True,
            'history_preserved':True,'input_closure_complete':False,'shipping_authority':False}

    def test_publication_store_receipt_distinguishes_diagnostic_snapshots_from_executed_units(self):
        value=self.publication_store();self.assertEqual(planner.validate_publication_store(value),value)
        for field,replacement in (('history_records',4),('unit_producers',5),('diagnostic_producers',0),
                ('invalidation_before_activation',False),('original_unit_cleanup',False),('history_preserved',False),
                ('last_affected',[]),('shipping_authority',True)):
            with self.subTest(field=field),self.assertRaises(planner.FreshnessHold):
                planner.validate_publication_store(value|{field:replacement})

    def cargo_graph(self):
        admissions=[]
        for ordinal,name,kind,source in ((7,'build-script-build','custom-build','build.rs'),
                (8,'private_cargo_probe','lib','src/lib.rs'),(9,'private-cargo-probe','bin','src/main.rs'),
                (14,'private_cargo_probe','lib','src/lib.rs')):
            admissions.append({'ordinal':ordinal,'target':{'package_id':'fixed-package','package_name':'private-cargo-probe',
                'version':'0.0.0','manifest_path':'/workspace/Cargo.toml','manifest_sha256':'1'*64,
                'target_name':name,'target_kind':[kind],'source_path':'/workspace/'+source,
                'source_sha256':'2'*64,'metadata_sha256':'3'*64}})
        return {'version':1,'metadata_sha256':'3'*64,'packages':1,'targets':3,'original_metadata_cleanup':True,'graph_dispatch':True,
            'admissions':admissions,'input_closure_complete':False,'shipping_authority':False}

    def test_cargo_graph_receipt_requires_every_compiler_binding_and_original_metadata_cleanup(self):
        value=self.cargo_graph();initial=[{'ordinal':i,'uid':2000+i} for i in range(5,14)];retry=[{'ordinal':14,'returncode':1}]
        self.assertEqual(planner.validate_cargo_graph(value,initial_units=initial,retry_units=retry,selected_uid=2008),value)
        for field,replacement in (('original_metadata_cleanup',False),('graph_dispatch',False),('packages',True),('targets',0),
                ('admissions',value['admissions'][:-1]),('input_closure_complete',True),('shipping_authority',True)):
            with self.subTest(field=field),self.assertRaises(planner.FreshnessHold):
                planner.validate_cargo_graph(value|{field:replacement},initial_units=initial,retry_units=retry,selected_uid=2008)
        for field,replacement in (('metadata_sha256','4'*64),('manifest_path','/tmp/Cargo.toml'),
                ('source_path','/workspace/other.rs'),('target_name','other')):
            bad=copy.deepcopy(value);bad['admissions'][0]['target'][field]=replacement
            with self.subTest(field=field),self.assertRaises(planner.FreshnessHold):
                planner.validate_cargo_graph(bad,initial_units=initial,retry_units=retry,selected_uid=2008)

    def test_actual_retry_requires_compiler_failure_and_original_planner_drain(self):
        value=self.cargo_retry()
        self.assertEqual(planner.validate_cargo_retry(value,start=14),value)
        for field,replacement in (('cargo_exit',0),('compiler_calls',True),('compiler_calls',0),('diagnostic','OLD'),
                ('rounds',True),('original_planner_cleanup',False),('original_unit_cleanup',False),
                ('unaffected_fingerprints_preserved',False),('original_generated_preserved',False),
                ('failed_outputs_unpublished',False),('pending',[]),('input_closure_complete',True),('shipping_authority',True)):
            with self.subTest(field=field),self.assertRaises(planner.FreshnessHold):
                planner.validate_cargo_retry(value|{field:replacement},start=14)
        for field,replacement in (('returncode',0),('original_cleanup',False),('started',False),('ordinal',13),('uid',1001)):
            bad=copy.deepcopy(value);bad['units'][0][field]=replacement
            with self.subTest(field=field),self.assertRaises(planner.FreshnessHold):planner.validate_cargo_retry(bad,start=14)

    def test_compiler_invalidation_report_requires_changed_content_and_durable_dependents(self):
        value=self.compiler_invalidation()
        self.assertEqual(planner.validate_compiler_invalidation(value),value)
        for field,replacement in (('unit_pid',True),('unit_uid',1001),('queries',0),('queries',True),
                ('binding_sha256','unknown'),('refreshed_sha256',value['original_sha256']),('pending',['library']),
                ('durable',False),('original_generated_preserved',False),('input_closure_complete',True),('shipping_authority',True)):
            with self.subTest(field=field),self.assertRaises(planner.FreshnessHold):
                planner.validate_compiler_invalidation(value|{field:replacement})

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
            'filesystem':filesystem_case,'statx_sha256':'1'*64})
        filesystem={'version':1,'input_bytes_preserved':True,'input_inode_metadata_preserved':True,
            'next_publication_distinct':True,'namespaces':{name:{'controller':name+':[1]','unit':name+':[2]'}
                for name in ('mnt','ipc','net')},'outputs':[['2e','directory',0o555],
                [b'unit-output'.hex(),'directory',0o555],[b'unit-output/empty'.hex(),'directory',0o555],
                [b'unit-output/generated-42.bin'.hex(),'file',0o444,14,hashlib.sha256(b'sealed result\n').hexdigest()]],
            'original_unit_cleanup':True,'allocation_removed':True,'shipping_authority':False}
        units=[{'ordinal':i,'uid':2000+i,'started':i!=2,'cancelled':i==2,'returncode':(-9 if i==2 else -10 if i==3 else 0),
            'original_cleanup':True,'input_closure_complete':False,'shipping_authority':False} for i in range(5)]
        value={'version':3,'kind':'ci-planner-protocol-probe','binding':binding,'command':command,
            'credential_roles':{'worker_rejects_planner':True,'planner_rejects_worker':True},
            'cases':cases,'units':units,'filesystem':filesystem,'guest_worker_quiescence':scopes[2],
            'input_closure_complete':False,'cargo_metadata_isolation_established':False,'shipping_authority':False}
        value['kernel_observation']={'version':1,'kind':'kernel-copied-filename-probe','binding':binding,'command':command,
            'shipping_authority':False,'kernel_release':'7.0.0-test','unit_pid':42,'unit_uid':2004,
            'program_sha256':'1'*64,'binary_sha256':'2'*64,'witness_observed':True,'positive_query_observed':True,
            'negative_query_observed':True,'max_path_query_observed':True,'compiler_queries_observed':True,'library_generated_query_observed':True,'after_kernel_copy':True,'events':3,'original_observer_cleanup':True,
            'filesystem_coverage_complete':False,'missed_probe_census_complete':True,'input_closure_complete':False,
            'allocation_removed':True}
        value['kernel_observation'].update(directory_reads=14,directory_entries=17,binary_path_query_observed=True,
            lookup_base_attempts=4,lookup_base_query_observed=True,lookup_changed_cwd_observed=True,lookup_invalid_base_observed=True,
            metadata_queries=8,path_metadata_query_observed=True,statx_copy_verified=True,
            path_metadata_failed_copy_observed=True,path_metadata_lookup_failure_observed=True,fstat_query_observed=True,fstat_invalid_fd_observed=True,fstat_failed_copy_observed=True,
            directory_cursor_query_observed=True,
            directory_witness_observed=True,binary_directory_name_observed=True,
            directory_short_read_observed=True,directory_eof_observed=True,cargo_directory_queries_observed=True)
        value['kernel_observation']['content_selection']={'queries':2,'binding_sha256':'1'*64,
            'content_sha256':'2'*64,'unrelated_sha256':'2'*64,'introduced_sha256':'3'*64,
            'kernel_bases_bound':True,'unrelated_content_unchanged':True,'negative_lookup_changed_identity':True,
            'input_closure_complete':False,'shipping_authority':False}
        census={'pid':100,'birth':1000,
            'programs':[{'id':i,'type':17 if i in (1,4) else 2,'tag':f'{i:016x}','recursion_misses':0} for i in range(1,5)],
            'links':[{'id':10+i,'type':'raw_tracepoint' if i in (1,4) else 'kretprobe_multi',
                'prog_id':i,'tag':f'{i:016x}',**({'tracepoint':'sys_enter' if i==1 else 'sys_exit'} if i in (1,4)
                    else {'function':'getname_flags','count':1,'flags':1,'missed':0})} for i in range(1,5)]}
        for i,(function,flags) in enumerate((('iterate_dir',0),('iterate_dir',1),('filldir64',0),('filldir64',1),('vfs_fstat',0),('vfs_fstat',1),('cp_new_stat',0),('cp_new_stat',1),('cp_statx',0),('cp_statx',1),('path_init',0),('path_init',1)),5):
            census['programs'].append({'id':i,'type':2,'tag':f'{i:016x}','recursion_misses':0})
            census['links'].append({'id':10+i,'type':'kretprobe_multi' if flags else 'kprobe_multi',
                'prog_id':i,'tag':f'{i:016x}','function':function,'count':1,'flags':flags,'missed':0})
        value['kernel_observation']['probe_census']={'before':census,'after':json.loads(json.dumps(census))}
        return value,binding,command,workers,writer,source

    def test_probe_requires_original_distinct_scope_roles_bindings_and_no_authority(self):
        original,binding,command,workers,writer,source=self.probe()
        self.assertEqual(planner.validate_probe(original,binding,command,workers,writer,source),original)
        for fault in ('role-credential-missing','role-credential-false','role-credential-int','version','binding','command','scope','writer','source','role','cancelled-start',
                'cleanup','case-role','control-fds','jobserver','closure','authority','unknown',
                'signal-status','signal-proxy','signal-number','signal-not-started','filesystem-namespace',
                'filesystem-output','filesystem-cleanup','filesystem-removed','filesystem-authority','filesystem-case',
                'kernel-binding','kernel-witness','kernel-loss','kernel-coverage','kernel-closure','kernel-cleanup','kernel-missed','kernel-census','kernel-binary-path','kernel-cursor','kernel-fstat','kernel-statx','kernel-base','kernel-content'):
            value=copy.deepcopy(original)
            if fault=='role-credential-missing':value.pop('credential_roles')
            elif fault=='role-credential-false':value['credential_roles']['worker_rejects_planner']=False
            elif fault=='role-credential-int':value['credential_roles']['planner_rejects_worker']=1
            elif fault=='version':value['version']=True
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
            elif fault=='kernel-cursor':value['kernel_observation']['directory_cursor_query_observed']=False
            elif fault=='kernel-fstat':value['kernel_observation']['fstat_failed_copy_observed']=False
            elif fault=='kernel-statx':value['kernel_observation']['statx_copy_verified']=False
            elif fault=='kernel-base':value['kernel_observation']['lookup_changed_cwd_observed']=False
            elif fault=='kernel-content':value['kernel_observation']['content_selection']['introduced_sha256']='2'*64
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
