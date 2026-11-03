"""Real pipe/descriptor tests for the protected planner's bounded stdin stager."""
import errno
import fcntl
import hashlib
import importlib.util
import math
import os
from pathlib import Path
import socket
import tempfile
import threading
import time
import unittest
from unittest.mock import patch


spec=importlib.util.spec_from_file_location('ci_cargo_staging',Path(__file__).resolve().parents[1]/'ci-cargo-staging.py')
staging=importlib.util.module_from_spec(spec);spec.loader.exec_module(staging)


class CargoStagingTests(unittest.TestCase):
    def setUp(self):
        self.cancel,self.cancel_writer=os.pipe2(os.O_CLOEXEC)
        self.addCleanup(os.close,self.cancel);self.addCleanup(os.close,self.cancel_writer)

    def capture(self,source,*,seconds=1,**kwargs):
        return staging.capture(source,self.cancel,command_deadline=time.monotonic()+seconds,**kwargs)

    def pipe(self,data,*,eof=True):
        reader,writer=os.pipe2(os.O_CLOEXEC)
        self.addCleanup(os.close,reader)
        os.write(writer,data)
        if eof:os.close(writer)
        else:self.addCleanup(os.close,writer)
        return reader

    def test_exact_pipe_bytes_are_sealed_and_cannot_be_rewritten(self):
        data=b'Rust stdin\0\xff\n'
        reader=self.pipe(data)
        with self.capture(reader) as captured:
            self.assertEqual(os.read(captured.fileno(),len(data)+1),data)
            self.assertEqual(captured.observation(),{'version':3,'kind':'ci-unit-stdin',
                'size_bytes':len(data),'sha256':hashlib.sha256(data).hexdigest(),
                'input_closure_complete':False,'shipping_authority':False})
            self.assertTrue(fcntl.fcntl(captured.fileno(),fcntl.F_GETFD)&fcntl.FD_CLOEXEC)
            for operation in (lambda:os.pwrite(captured.fileno(),b'changed',0),
                    lambda:os.ftruncate(captured.fileno(),0)):
                with self.assertRaises(OSError) as seen:operation()
                self.assertEqual(seen.exception.errno,errno.EPERM)
        with self.assertRaises(staging.StagingUnavailable):captured.fileno()

    def test_regular_file_and_dev_null_are_frozen_without_changing_caller_flags(self):
        with tempfile.TemporaryFile() as source:
            source.write(b'file bytes');source.seek(0)
            before=fcntl.fcntl(source.fileno(),fcntl.F_GETFL)
            with self.capture(source.fileno()) as captured:
                self.assertEqual(os.read(captured.fileno(),99),b'file bytes')
            self.assertEqual(fcntl.fcntl(source.fileno(),fcntl.F_GETFL),before)
        with open('/dev/null','rb') as source,self.capture(source.fileno()) as captured:
            self.assertEqual(captured.observation()['size_bytes'],0)

    def test_full_eight_mib_requires_eof_and_one_extra_byte_is_refused(self):
        for count in (staging.MAX_BYTES,staging.MAX_BYTES+1):
            with self.subTest(count=count),tempfile.TemporaryFile() as source:
                source.write(b'x'*count);source.seek(0)
                if count==staging.MAX_BYTES:
                    with self.capture(source.fileno()) as captured:
                        self.assertEqual(captured.observation()['size_bytes'],count)
                else:
                    with self.assertRaises(staging.StagingUnavailable) as seen:self.capture(source.fileno())
                    self.assertEqual(seen.exception.stage_reason,'stdin_limit')

    def test_full_pipe_is_drained_with_backpressure_before_publication(self):
        reader,writer=os.pipe2(os.O_CLOEXEC);os.set_blocking(writer,False)
        stopped=threading.Event();errors=[];remaining_bytes=staging.MAX_BYTES
        def produce():
            remaining=remaining_bytes
            try:
                while remaining and not stopped.is_set():
                    try:remaining-=os.write(writer,b'\xff'*min(65536,remaining))
                    except BlockingIOError:stopped.wait(.001)
            except OSError as error:
                if not stopped.is_set():errors.append(error)
            finally:os.close(writer)
        thread=threading.Thread(target=produce);thread.start()
        try:
            with self.capture(reader,seconds=3) as captured:
                self.assertEqual(captured.observation()['size_bytes'],remaining_bytes)
                self.assertEqual(captured.observation()['sha256'],hashlib.sha256(b'\xff'*remaining_bytes).hexdigest())
            self.assertTrue(os.get_blocking(reader))
        finally:
            stopped.set();os.close(reader);thread.join(timeout=2)
        self.assertFalse(thread.is_alive());self.assertEqual(errors,[])

    def test_no_eof_cannot_publish_an_input_identity(self):
        with self.assertRaises(staging.StagingUnavailable) as seen:self.capture(self.pipe(b'partial',eof=False),seconds=.04)
        self.assertEqual(seen.exception.stage_reason,'stdin_deadline')

    def test_cancelled_scope_refuses_even_an_already_readable_eof(self):
        os.write(self.cancel_writer,b'cancel')
        with self.assertRaises(staging.StagingUnavailable) as seen:self.capture(self.pipe(b'ready'))
        self.assertEqual(seen.exception.stage_reason,'stdin_cancelled')

    def test_cancellation_wakes_staging_without_waiting_for_source_eof(self):
        reader=self.pipe(b'',eof=False)
        def cancel():
            time.sleep(.03);os.write(self.cancel_writer,b'cancel')
        thread=threading.Thread(target=cancel);thread.start()
        try:
            with self.assertRaises(staging.StagingUnavailable) as seen:self.capture(reader)
            self.assertEqual(seen.exception.stage_reason,'stdin_cancelled')
        finally:thread.join(timeout=2)
        self.assertFalse(thread.is_alive())

    def test_closed_cancellation_channel_is_a_hold(self):
        reader,writer=os.pipe2(os.O_CLOEXEC);os.close(writer)
        try:
            with self.assertRaises(staging.StagingUnavailable) as seen:
                staging.capture(self.pipe(b'ready'),reader,command_deadline=time.monotonic()+1)
            self.assertEqual(seen.exception.stage_reason,'stdin_cancelled')
        finally:os.close(reader)

    def test_a_cancel_between_eof_and_sealing_cannot_publish(self):
        original=staging._seal
        def cancel_then_seal(descriptor):
            os.write(self.cancel_writer,b'cancel');return original(descriptor)
        with patch.object(staging,'_seal',side_effect=cancel_then_seal):
            with self.assertRaises(staging.StagingUnavailable) as seen:self.capture(self.pipe(b'bytes'))
        self.assertEqual(seen.exception.stage_reason,'stdin_cancelled')

    def test_no_memfd_leaks_on_repeated_refusal_or_io_failure(self):
        reader=self.pipe(b'data',eof=False)
        before=len(os.listdir('/proc/self/fd'))
        for _ in range(3):
            with self.assertRaises(staging.StagingUnavailable):self.capture(reader,seconds=.015)
        self.assertEqual(len(os.listdir('/proc/self/fd')),before)
        source=self.pipe(b'complete')
        with patch.object(staging,'_seal',side_effect=OSError(errno.ENOSPC,'injected')):
            with self.assertRaises(OSError) as seen:self.capture(source)
            self.assertEqual(seen.exception.errno,errno.ENOSPC)
        self.assertEqual(len(os.listdir('/proc/self/fd')),before+1)

    def test_unbounded_devices_and_socket_stdin_are_refused(self):
        with open('/dev/zero','rb') as source:
            with self.assertRaises(staging.StagingUnavailable):self.capture(source.fileno())
        first,second=socket.socketpair()
        with first as source,second:
            with self.assertRaises(staging.StagingUnavailable):self.capture(source.fileno())

    def test_bad_deadline_and_shared_source_control_descriptor_are_refused(self):
        reader=self.pipe(b'bytes')
        for value in (True,False,None,'1',math.nan,math.inf,-math.inf):
            with self.subTest(value=value),self.assertRaises(staging.StagingUnavailable):
                staging.capture(reader,self.cancel,command_deadline=value)
        with self.assertRaises(staging.StagingUnavailable):
            staging.capture(reader,reader,command_deadline=time.monotonic()+1)
        duplicate=os.dup(reader)
        try:
            with self.assertRaises(staging.StagingUnavailable):
                staging.capture(reader,duplicate,command_deadline=time.monotonic()+1)
        finally:os.close(duplicate)

    def test_replaced_captured_descriptor_cannot_close_an_unrelated_controller_descriptor(self):
        captured=self.capture(self.pipe(b'bytes'));number=captured.fileno();os.close(number)
        replacement=os.open('/dev/null',os.O_RDONLY|os.O_CLOEXEC)
        try:
            self.assertEqual(replacement,number,'fixture must replace the same descriptor number')
            with self.assertRaises(staging.StagingUnavailable):captured.fileno()
            with self.assertRaises(staging.StagingUnavailable):captured.close()
            self.assertEqual(os.read(replacement,1),b'')
        finally:os.close(replacement)

    def test_already_expired_original_command_cannot_start_staging(self):
        with self.assertRaises(staging.StagingUnavailable) as seen:
            staging.capture(self.pipe(b'ready'),self.cancel,command_deadline=time.monotonic()-1)
        self.assertEqual(seen.exception.stage_reason,'stdin_deadline')

    def jobserver(self):
        reader,writer=os.pipe2(os.O_CLOEXEC)
        self.addCleanup(os.close,reader);self.addCleanup(os.close,writer)
        return {71:reader,72:writer}

    def test_jobserver_changes_only_transport_numbers_and_retains_real_pipe_capabilities(self):
        descriptors=self.jobserver();environment={b'CARGO_MAKEFLAGS':b'-j --jobserver-fds=71,72 --jobserver-auth=71,72',
            b'MAKEFLAGS':b' --jobserver-auth=71,72  -j ',b'RAW':b'\xffbytes'}
        original=dict(environment)
        normalized,mapping,observation=staging.normalize_jobserver(environment,descriptors)
        self.assertEqual(environment,original)
        self.assertEqual(normalized,{b'CARGO_MAKEFLAGS':b'-j --jobserver-fds=3,4 --jobserver-auth=3,4',
            b'MAKEFLAGS':b' --jobserver-auth=3,4  -j ',b'RAW':b'\xffbytes'})
        self.assertEqual(mapping,((3,descriptors[71]),(4,descriptors[72])))
        os.write(mapping[1][1],b'+');self.assertEqual(os.read(mapping[0][1],1),b'+')
        self.assertFalse(observation['shipping_authority']);self.assertFalse(observation['input_closure_complete'])

    def test_different_request_descriptor_numbers_keep_the_same_semantic_environment(self):
        descriptors=self.jobserver();other={161:descriptors[71],162:descriptors[72]}
        first=staging.normalize_jobserver({b'CARGO_MAKEFLAGS':b'-j --jobserver-auth=71,72'},descriptors)
        second=staging.normalize_jobserver({b'CARGO_MAKEFLAGS':b'-j --jobserver-auth=161,162'},other)
        self.assertEqual(first,second)

    def test_unknown_inherited_fifo_missing_pipe_end_and_unbound_jobserver_are_refused(self):
        descriptors=self.jobserver();flags={b'CARGO_MAKEFLAGS':b'-j --jobserver-auth=71,72'}
        for environment,mapping in ((flags,{71:descriptors[71]}),(flags,{**descriptors,73:descriptors[71]}),
                ({},descriptors),(flags,{})):
            with self.subTest(environment=environment,mapping=mapping),self.assertRaises(staging.StagingUnavailable):
                staging.normalize_jobserver(environment,mapping)

    def test_conflicting_ambiguous_or_named_jobserver_flags_are_refused(self):
        descriptors=self.jobserver()
        for value in (b'--jobserver-auth=71,73',b'--jobserver-auth=fifo:/tmp/jobs',b'--jobserver-auth=',
                b'--jobserver-auth=71,72 --jobserver-auth=71,72',b'--jobserver-auth 71,72',
                b'"--jobserver-auth=71,72"',b'--jobserver-fds=72,71 --jobserver-auth=71,72'):
            with self.subTest(value=value),self.assertRaises(staging.StagingUnavailable):
                staging.normalize_jobserver({b'CARGO_MAKEFLAGS':value},descriptors)
        with self.assertRaises(staging.StagingUnavailable):
            staging.normalize_jobserver({b'CARGO_MAKEFLAGS':b'--jobserver-auth=71,72',
                b'MAKEFLAGS':b'--jobserver-auth=72,71'},descriptors)

    def test_jobserver_pipe_ends_must_be_the_same_kernel_pipe_in_the_right_directions(self):
        first=self.jobserver();second=self.jobserver();flags={b'CARGO_MAKEFLAGS':b'--jobserver-auth=71,72'}
        for mapping in ({71:first[72],72:first[71]},{71:first[71],72:second[72]}):
            with self.subTest(mapping=mapping),self.assertRaises(staging.StagingUnavailable):
                staging.normalize_jobserver(flags,mapping)
        with tempfile.TemporaryFile() as regular,self.assertRaises(staging.StagingUnavailable):
            staging.normalize_jobserver(flags,{71:first[71],72:regular.fileno()})

    def test_received_jobserver_copies_must_be_cloexec_until_the_controlled_tool_handoff(self):
        descriptors=self.jobserver();os.set_inheritable(descriptors[71],True)
        try:
            with self.assertRaises(staging.StagingUnavailable):
                staging.normalize_jobserver({b'CARGO_MAKEFLAGS':b'--jobserver-auth=71,72'},descriptors)
        finally:os.set_inheritable(descriptors[71],False)

    def test_no_jobserver_does_not_add_or_reinterpret_any_environment_bytes(self):
        environment={b'RAW':b'\xff\0data',b'MAKEFLAGS':b'-j4'}
        with self.assertRaises(staging.StagingUnavailable):staging.normalize_jobserver(environment,{})
        environment[b'RAW']=b'\xffdata'
        normalized,mapping,observation=staging.normalize_jobserver(environment,{})
        self.assertEqual(normalized,environment);self.assertIsNot(normalized,environment)
        self.assertEqual(mapping,());self.assertIsNone(observation)


if __name__=='__main__':unittest.main()
