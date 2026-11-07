"""Real Git/filesystem source projection; privileged mount/UID checks are separate."""
import errno
import importlib.util
import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import unittest
import uuid
from unittest.mock import patch


spec=importlib.util.spec_from_file_location('source_projection_cache',Path(__file__).resolve().parents[1]/'ci-cache-generation.py')
cache=importlib.util.module_from_spec(spec);spec.loader.exec_module(cache)


class SourceProjectionTests(unittest.TestCase):
    def setUp(self):
        self.directory=self.enterContext(tempfile.TemporaryDirectory());self.root=Path(self.directory)
        self.source=self.root/'source';self.source.mkdir(mode=0o700)
        self.git('init','-q','--template=');(self.source/'src').mkdir(mode=0o700)
        (self.source/'src/main.rs').write_bytes(b'fn main(){println!("ORIGINAL");}\n')
        script=self.source/'run.sh';script.write_bytes(b'#!/bin/sh\necho ORIGINAL\n');script.chmod(0o700)
        self.git('add','.');self.git('commit','-qm','source')
        self.head=self.git('rev-parse','HEAD');self.git('update-ref','refs/remotes/origin/main',self.head)
        self.objects=self.source/'.git/objects'
        self.binding={'generation':str(uuid.uuid4()),'head':self.head,'tree':self.git('rev-parse','HEAD^{tree}'),
            'contract_sha256':'c'*64,'authority_sha256':'d'*64,'cache_identity_sha256':'e'*64,
            'source_inputs_sha256':cache.source_census(self.source,self.objects,self.head)}
        self.command=str(uuid.uuid4());self.image=self.root/'cache';self.image.mkdir(mode=0o700)
        self.target=cache.prepare(self.image,self.binding,self.command,os.getuid(),os.getgid())

    def git(self,*args,cwd=None):
        return subprocess.check_output(['git','-c','core.hooksPath=/dev/null','-c','core.fsmonitor=false',
            '-c','user.name=Fixture','-c','user.email=fixture@example.invalid','-c','commit.gpgsign=false',
            '-C',str(cwd or self.source),*args],text=True,env={'PATH':os.defpath,
            'GIT_CONFIG_NOSYSTEM':'1','GIT_CONFIG_GLOBAL':'/dev/null'}).strip()

    def project(self,**kwargs):
        return cache.prepare_source_projection(self.target,self.source,self.objects,self.binding,self.command,{},**kwargs)

    def test_projection_is_git_bound_self_contained_read_only_and_does_not_chmod_export(self):
        before=stat.S_IMODE(self.source.stat().st_mode);old=946684800000000000
        os.utime(self.source/'src/main.rs',ns=(old,old))
        projected=self.project()
        self.assertEqual((projected/'src/main.rs').read_bytes(),(self.source/'src/main.rs').read_bytes())
        self.assertEqual((projected/'src/main.rs').stat().st_mtime_ns,old)
        self.assertEqual(stat.S_IMODE(self.source.stat().st_mode),before)
        self.assertEqual(stat.S_IMODE(projected.stat().st_mode),0o555)
        self.assertEqual(stat.S_IMODE((projected/'src/main.rs').stat().st_mode),0o444)
        self.assertEqual(stat.S_IMODE((projected/'run.sh').stat().st_mode),0o555)
        self.assertFalse((projected/'.git/objects/info/alternates').exists())
        # The original object store disappears. Git must use the projection's
        # own packed objects and preserved controlled reference instead.
        self.objects.rename(self.source/'.git/objects-away')
        self.assertEqual(self.git('show','HEAD:src/main.rs',cwd=projected),'fn main(){println!("ORIGINAL");}')
        self.assertEqual(self.git('rev-parse','refs/remotes/origin/main',cwd=projected),self.head)
        self.assertEqual(cache.source_census(projected,projected/'.git/objects',self.head,
            generated_directories=['target']),self.binding['source_inputs_sha256'])
        record=cache._read_record(self.target.parent/'source-projection.json',max_bytes=16*1024**2)
        self.assertEqual(record['binding'],self.binding);self.assertEqual(record['state'],'sealed')
        self.assertFalse(record['shipping_authority'])

    def test_candidate_git_configuration_and_hooks_are_not_copied_or_executed(self):
        marker=self.root/'must-not-run'
        self.git('config','core.fsmonitor','touch '+str(marker))
        self.git('config','alias.host-command','!touch '+str(marker))
        hook=self.source/'.git/hooks';hook.mkdir(exist_ok=True);(hook/'post-checkout').write_text('touch '+str(marker))
        with self.assertRaises(cache.FreshnessHold):self.project()
        self.assertFalse((self.target.parent/'source-projection').exists());self.assertFalse(marker.exists())

    def test_privileged_projection_can_read_only_the_explicitly_admitted_foreign_owner_repository(self):
        # Exercise Git's actual ownership check without needing root on the
        # host. In the guest the controller is root and the 9p export is UID 1000.
        original=subprocess.Popen
        def foreign_owner(*args,**kwargs):
            kwargs['env']={**kwargs.get('env',{}),'GIT_TEST_ASSUME_DIFFERENT_OWNER':'1'}
            return original(*args,**kwargs)
        with patch.object(subprocess,'Popen',side_effect=foreign_owner):
            projected=self.project()
        self.assertEqual(self.git('show','HEAD:src/main.rs',cwd=projected),'fn main(){println!("ORIGINAL");}')

    def test_changed_git_bytes_or_wrong_active_command_cannot_allocate_projection(self):
        (self.source/'src/main.rs').write_text('changed source')
        with self.assertRaises(cache.FreshnessHold):self.project()
        self.assertFalse((self.target.parent/'source-projection').exists())
        self.assertFalse((self.target.parent/'source-projection.json').exists())
        self.git('checkout','--','src/main.rs');old=self.command;self.command=str(uuid.uuid4())
        with self.assertRaises(cache.FreshnessHold):self.project()
        self.command=old

    def test_reuse_checks_sealed_bytes_without_overwriting_unknown_or_modified_data(self):
        projected=self.project();self.assertEqual(self.project(),projected)
        file=projected/'src/main.rs';file.chmod(0o600);file.write_text('tampered');file.chmod(0o444)
        with self.assertRaises(cache.FreshnessHold):self.project()
        self.assertEqual(file.read_text(),'tampered')

    def test_interrupted_projection_is_preserved_then_reclaimed_under_original_generation(self):
        with patch.object(cache,'_seal_source_projection',side_effect=OSError(errno.EIO,'injected')):
            with self.assertRaises(OSError):self.project()
        projected=self.target.parent/'source-projection';self.assertTrue(projected.is_dir())
        with self.assertRaises(cache.FreshnessHold):self.project()
        self.assertEqual(cache.reclaim(self.image,[self.binding['generation']],os.getuid(),os.getgid()),[])
        self.assertEqual(cache.reclaim(self.image,[],os.getuid(),os.getgid()),[self.binding['generation']])
        self.assertTrue((self.source/'src/main.rs').exists())

    def test_retirement_replays_partial_deletion_and_preserves_original_projection_inventory(self):
        projected=self.project();original=cache._read_record(self.target.parent/'source-projection.json',max_bytes=16*1024**2)
        def interrupted(path):
            file=projected/'src/main.rs';file.unlink();raise OSError(errno.EIO,'after one file')
        with patch.object(cache.shutil,'rmtree',side_effect=interrupted):
            with self.assertRaises(cache.FreshnessHold):cache.reclaim(self.image,[],os.getuid(),os.getgid())
        receipt=self.image/'ci-generation-retirements'/(self.binding['generation']+'.json')
        self.assertEqual(cache._read_record(receipt,max_bytes=16*1024**2)['source_projection'],original)
        self.assertEqual(cache.reclaim(self.image,[],os.getuid(),os.getgid()),[self.binding['generation']])
        self.assertEqual(cache._read_record(receipt,max_bytes=16*1024**2)['source_projection'],original)

    def test_projection_retirement_refuses_extra_files_and_links_to_external_data(self):
        projected=self.project();projected.chmod(0o700);(projected/'unknown').write_text('unknown')
        projected.chmod(0o555)
        self.assertEqual(cache.reclaim(self.image,[],os.getuid(),os.getgid()),[])
        self.assertTrue((projected/'unknown').exists())

    def test_projection_cannot_be_created_after_generation_retirement_was_fenced(self):
        retired=self.image/'ci-generation-retirements';retired.mkdir(mode=0o700)
        cache._write_record(retired/(self.binding['generation']+'.json'),{'do-not-reuse':True})
        with self.assertRaises(cache.FreshnessHold):self.project()
        self.assertFalse((self.target.parent/'source-projection').exists())

    def test_projection_does_not_export_dangling_git_objects(self):
        data=b'synthetic unreferenced object, not an input'
        oid=subprocess.check_output(['git','-C',str(self.source),'hash-object','-w','--stdin'],input=data).decode().strip()
        projected=self.project()
        result=subprocess.run(['git','-C',str(projected),'cat-file','-e',oid],capture_output=True,
            env={'PATH':os.defpath,'GIT_CONFIG_NOSYSTEM':'1','GIT_CONFIG_GLOBAL':'/dev/null'})
        self.assertNotEqual(result.returncode,0)

    def test_large_original_inventory_survives_retirement_without_changing_control_record_limit(self):
        for index in range(120):(self.source/('input-'+str(index)+'.rs')).write_text('input '+str(index))
        self.git('add','.');self.git('commit','-qm','larger fixture')
        self.head=self.git('rev-parse','HEAD');self.binding.update(head=self.head,tree=self.git('rev-parse','HEAD^{tree}'),
            source_inputs_sha256=cache.source_census(self.source,self.objects,self.head))
        cache._write_record(self.target.parent/'control.json',{'version':1,'binding':self.binding,'active_command':self.command})
        self.project();record=self.target.parent/'source-projection.json'
        self.assertGreater(record.stat().st_size,8192)
        with self.assertRaises(cache.FreshnessHold):cache._read_record(record)
        original=cache._read_record(record,max_bytes=16*1024**2)
        self.assertEqual(cache.reclaim(self.image,[],os.getuid(),os.getgid()),[self.binding['generation']])
        retired=self.image/'ci-generation-retirements'/(self.binding['generation']+'.json')
        self.assertEqual(cache._read_record(retired,max_bytes=16*1024**2)['source_projection'],original)

    def test_linked_or_missing_projection_metadata_never_authorizes_deletion(self):
        for kind in ('missing','linked','hardlinked-source'):
            with self.subTest(kind=kind):
                binding={**self.binding,'generation':str(uuid.uuid4())}
                target=cache.prepare(self.image,binding,self.command,os.getuid(),os.getgid())
                projected=cache.prepare_source_projection(target,self.source,self.objects,binding,self.command,{})
                record=target.parent/'source-projection.json'
                if kind=='missing':record.unlink()
                elif kind=='linked':
                    outside=self.root/'outside-record';record.rename(outside);record.symlink_to(outside)
                else:os.link(projected/'src/main.rs',self.root/'external-link')
                self.assertEqual(cache.reclaim(self.image,[self.binding['generation']],os.getuid(),os.getgid()),[])
                self.assertTrue(projected.exists())

    def test_quota_failures_during_retirement_preserve_resource_errno_and_original_intent(self):
        self.project()
        for number in (errno.ENOSPC,errno.EDQUOT):
            with self.subTest(errno=number),patch.object(cache.shutil,'rmtree',side_effect=OSError(number,'injected')):
                with self.assertRaises(OSError) as seen:cache.reclaim(self.image,[],os.getuid(),os.getgid())
                self.assertEqual(seen.exception.errno,number)
        self.assertEqual(cache.reclaim(self.image,[],os.getuid(),os.getgid()),[self.binding['generation']])

    def test_sha256_git_objects_remain_self_contained(self):
        source=self.root/'sha256';source.mkdir(mode=0o700)
        self.git('init','-q','--template=','--object-format=sha256',cwd=source)
        (source/'input').write_text('sha256 input');self.git('add','.',cwd=source);self.git('commit','-qm','sha256',cwd=source)
        head=self.git('rev-parse','HEAD',cwd=source)
        binding={**self.binding,'generation':str(uuid.uuid4()),'head':head,
            'tree':self.git('rev-parse','HEAD^{tree}',cwd=source),
            'source_inputs_sha256':cache.source_census(source,source/'.git/objects',head)}
        target=cache.prepare(self.image,binding,self.command,os.getuid(),os.getgid())
        projected=cache.prepare_source_projection(target,source,source/'.git/objects',binding,self.command,{})
        self.assertEqual(self.git('show','HEAD:input',cwd=projected),'sha256 input')

    def test_shallow_history_boundary_is_preserved_without_copying_remote_configuration(self):
        (self.source/'src/main.rs').write_text('second source');self.git('add','.');self.git('commit','-qm','second')
        shallow=self.root/'shallow'
        subprocess.run(['git','clone','-q','--depth=1',self.source.as_uri(),str(shallow)],check=True,
            env={'PATH':os.defpath,'GIT_CONFIG_NOSYSTEM':'1','GIT_CONFIG_GLOBAL':'/dev/null'})
        # Match the controller export: transport/credential/branch configuration
        # from the original checkout never enters its minimal Git metadata.
        self.git('remote','remove','origin',cwd=shallow)
        head=self.git('rev-parse','HEAD',cwd=shallow)
        binding={**self.binding,'generation':str(uuid.uuid4()),'head':head,
            'tree':self.git('rev-parse','HEAD^{tree}',cwd=shallow),
            'source_inputs_sha256':cache.source_census(shallow,shallow/'.git/objects',head)}
        target=cache.prepare(self.image,binding,self.command,os.getuid(),os.getgid())
        projected=cache.prepare_source_projection(target,shallow,shallow/'.git/objects',binding,self.command,{})
        self.assertEqual(self.git('rev-list','--count','HEAD',cwd=projected),'1')
        self.assertNotIn(self.source.as_uri(),(projected/'.git/config').read_text())


if __name__=='__main__':unittest.main()
