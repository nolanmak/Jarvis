"""Read-only observations use the same limits and reservations as admission."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
spec=importlib.util.spec_from_file_location('resources',Path(__file__).parents[1]/'run-resource-scope.py')
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
MIB=1024**2

class ReadinessTests(unittest.TestCase):
    def test_limits_capacity_and_uninitialized_accounting_are_distinct_without_writes(self):
        for case in ('available','reservations','host','ceiling','invalid','delegation','uninitialized','busy'):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as tmp:
                parent=Path(tmp)/'fixture.service';parent.mkdir();leaf=parent/'daemon';leaf.mkdir()
                values={'cgroup.procs':'','cgroup.controllers':'cpu memory pids','cgroup.subtree_control':'cpu memory pids',
                    'memory.max':str(512*MIB),'memory.current':str(32*MIB),'pids.max':'64','cpu.max':'100000 100000'}
                for name,value in values.items():(parent/name).write_text(value)
                (leaf/'memory.current').write_text(str(32*MIB))
                policy={'memory_bytes':128*MIB,'pids':8,'cpu_percent':20}
                expected='available'
                if case=='reservations':
                    run=parent/('run-'+'a'*32);run.mkdir();(run/'memory.current').write_text('0');(run/'memory.max').write_text(str(400*MIB));expected='capacity-exhausted'
                if case=='host':expected='capacity-exhausted'
                if case=='ceiling':policy['memory_bytes']=1024*MIB;expected='declaration-exceeds-service'
                if case=='invalid':policy['pids']=0;expected='declaration-invalid'
                if case=='delegation':(parent/'cgroup.controllers').write_text('cpu pids');expected='delegation-unavailable'
                if case=='uninitialized':(parent/'cgroup.subtree_control').write_text('');expected='capacity-unverified'
                if case=='busy':expected='inspection-busy'
                before={str(p.relative_to(parent)):(p.stat().st_ino,p.stat().st_mtime_ns,p.read_bytes()) for p in parent.rglob('*') if p.is_file()}
                with patch.object(m,'current_group',return_value=leaf), patch.object(m,'available_memory',return_value=(100 if case=='host' else 4096)*MIB):
                    if case=='busy':
                        with patch.object(m.fcntl,'flock',side_effect=BlockingIOError()):report=m.inspect(policy)
                    else:report=m.inspect(policy)
                self.assertEqual(report['status'],expected,report)
                self.assertEqual(report['permits_attempt'],case in ('available','uninitialized'))
                self.assertFalse(report['live_verified'])
                after={str(p.relative_to(parent)):(p.stat().st_ino,p.stat().st_mtime_ns,p.read_bytes()) for p in parent.rglob('*') if p.is_file()}
                self.assertEqual(before,after)
                self.assertEqual(set(p.name for p in parent.iterdir()),set(values)|{'daemon'}|({run.name} if case=='reservations' else set()))
                if case=='reservations':self.assertEqual(report['refusal']['facts']['reserved_bytes'],432*MIB)
                if case=='invalid':
                    with self.assertRaises(m.AdmissionRefused):m.RunScope(policy)

class LiveReadinessTests(unittest.TestCase):
    @unittest.skipUnless(os.environ.get('AUTOSHIP_RESOURCE_READINESS_LIVE')=='1','requires isolated delegated 512MiB service')
    def test_idle_live_reservation_remains_charged_during_inspection(self):
        parent=m.current_group().parent
        request={'memory_bytes':384*MIB,'pids':8,'cpu_percent':20}
        def observe():
            before=(parent/'cgroup.subtree_control').read_text(),set(parent.glob('run-*'))
            report=m.inspect(request)
            self.assertEqual(before,((parent/'cgroup.subtree_control').read_text(),set(parent.glob('run-*'))))
            return report
        self.assertIn(observe()['status'],('available','capacity-unverified'))
        scope=m.RunScope({'memory_bytes':256*MIB,'pids':8,'cpu_percent':20})
        try:
            self.assertLess(int((scope.path/'memory.current').read_text()),16*MIB)
            report=observe()
            self.assertEqual(report['status'],'capacity-exhausted',report)
            self.assertGreaterEqual(report['refusal']['facts']['reserved_bytes'],256*MIB)
            self.assertTrue(scope.path.exists())
        finally:scope.close()
        self.assertEqual(observe()['status'],'available')

if __name__=='__main__':unittest.main()
