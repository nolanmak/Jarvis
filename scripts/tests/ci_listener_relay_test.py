"""Canonical transport bytes are data; non-root metadata cannot install policy."""
import importlib.util
import os
from pathlib import Path
import unittest

ROOT=Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('listener_relay_planner',ROOT/'scripts/ci-cargo-planner.py')
planner=importlib.util.module_from_spec(spec);spec.loader.exec_module(planner)
staging=planner.staging


class ListenerRelayTests(unittest.TestCase):
    def test_canonical_form_preserves_every_original_field_after_the_role_tag(self):
        for original,expected in ((b'JARVWRK3',b'JARVWRL3'),(b'JARVCFG3',b'JARVCFL3'),(b'JARVUNT3',b'JARVUNL3')):
            with self.subTest(role=original):
                body=original+bytes(range(152))
                self.assertEqual(staging.listener_relay_body(body),expected+body[8:])
                self.assertEqual(body[:8],original)

    def test_truncated_extended_foreign_and_already_transformed_inputs_are_refused(self):
        original=b'JARVWRK3'+bytes(range(152))
        for body in (original[:-1],original+b'x',b'JARVCFG2'+original[8:],b'JARVWRL3'+original[8:],bytearray(original)):
            with self.subTest(body_type=type(body),length=len(body)),self.assertRaises(staging.StagingUnavailable):
                staging.listener_relay_body(body)

    def memory_record(self,readonly):
        import errno
        return {'mount_readonly':readonly,'read_control':True,'write_results':{
            name:errno.EROFS if readonly else 0 for name in ('direct','self_root','victim_root','fd_alias')},
            'readonly_fd_write_errno':errno.EBADF,'victim_final_hex':(b'original' if readonly else b'writer-3').hex(),
            'victim_reaped':True}

    def test_memory_report_keeps_writable_control_distinct_from_guarded_results(self):
        for readonly in (False,True):
            row=self.memory_record(readonly)
            self.assertIs(planner.validate_proc_memory_case(row,readonly),row)
            with self.assertRaises(planner.FreshnessHold):planner.validate_proc_memory_case(row,not readonly)

    def test_memory_report_refuses_missing_alias_false_positive_or_authority_claim(self):
        for fault in ('missing-alias','permission-denied','boolean-errno','wrong-bytes','no-read-control','no-reap','authority'):
            row=self.memory_record(True)
            if fault=='missing-alias':row['write_results'].pop('fd_alias')
            elif fault=='permission-denied':row['write_results']['direct']=13
            elif fault=='boolean-errno':row['write_results']['direct']=True
            elif fault=='wrong-bytes':row['victim_final_hex']=b'writer-3'.hex()
            elif fault=='no-read-control':row['read_control']=False
            elif fault=='no-reap':row['victim_reaped']=False
            else:row['full_ci']=True
            with self.subTest(fault=fault),self.assertRaises(planner.FreshnessHold):planner.validate_proc_memory_case(row,True)

    @unittest.skipIf(os.geteuid()==0,'specifically verifies non-root refusal')
    def test_forged_python_activation_fields_cannot_transform_a_non_root_configuration(self):
        import fcntl
        fd=os.memfd_create('untrusted-listener-claim',os.MFD_CLOEXEC|os.MFD_ALLOW_SEALING)
        try:
            body=b'JARVWRK3'+bytes(range(152));os.write(fd,body);staging._seal(fd)
            flags=fcntl.fcntl(fd,fcntl.F_GETFD);before=os.fstat(fd)
            policy=object.__new__(planner.UnitPolicy);policy._activated=True;policy._listener_pid=os.getpid()
            with self.assertRaises(planner.FreshnessHold):policy.prepare_relay_configuration(fd)
            self.assertEqual(os.pread(fd,161,0),body)
            self.assertEqual(fcntl.fcntl(fd,fcntl.F_GETFD),flags)
            self.assertEqual((os.fstat(fd).st_dev,os.fstat(fd).st_ino),(before.st_dev,before.st_ino))
        finally:os.close(fd)


if __name__=='__main__':unittest.main()
