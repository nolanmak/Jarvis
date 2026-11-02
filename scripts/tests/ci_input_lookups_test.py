"""Experimental lookup decoding; no compiler-input or reuse authority."""
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


class LookupTests(unittest.TestCase):
    def setUp(self):
        path=Path(__file__).resolve().parents[1]/'ci-input-lookups.py'
        spec=importlib.util.spec_from_file_location('lookup_observation',path)
        self.module=importlib.util.module_from_spec(spec);spec.loader.exec_module(self.module)

    def trace(self,root):
        output=root/'trace';program='import os,sys\ntry:os.stat(sys.argv[1])\nexcept FileNotFoundError:pass\n'
        result=subprocess.run(['/usr/bin/strace','-f','-qq','-s','4096','-yy','-xx',
            '-e','trace=newfstatat,statx', '-o',str(output),sys.executable,'-I','-S','-c',program,
            str(root/'missing.rs')],capture_output=True,text=True,timeout=10)
        self.assertEqual(result.returncode,0,result.stderr)
        return output.read_bytes()

    def test_real_failed_lookup_is_retained_without_claiming_a_complete_input_set(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);raw=self.trace(root)
            events=self.module.failed_queries(raw)
            missing=[event for event in events if event['path']==os.fsencode(root/'missing.rs')]
            self.assertEqual(len(missing),1)
            self.assertEqual(missing[0]['errno'],'ENOENT')
            self.assertEqual(missing[0]['intent'],'metadata')
            observation=self.module.observation(events)
            self.assertFalse(observation['input_closure_complete'])
            self.assertFalse(observation['shipping_authority'])
            (root/'unrelated.rs').write_bytes(b'unrelated')
            self.assertEqual([event['path'] for event in self.module.failed_queries(self.trace(root))],
                [event['path'] for event in events])
            (root/'missing.rs').write_bytes(b'now present')
            self.assertFalse(any(event['path']==os.fsencode(root/'missing.rs')
                for event in self.module.failed_queries(self.trace(root))))

    def test_hex_paths_preserve_non_utf8_bytes_and_relative_symlink_components(self):
        raw=br'12 newfstatat(AT_FDCWD<\x2f\x77>, "\x61\x2f\x2e\x2e\x2f\xff", 0x1, 0) = -1 ENOENT (No such file or directory)'+b'\n'
        event=self.module.failed_queries(raw)[0]
        self.assertEqual(event['directory'],b'/w')
        self.assertEqual(event['path'],b'a/../\xff')

    def test_interleaved_unfinished_queries_keep_their_original_process_binding(self):
        raw=(br'12 stat("\x61",  <unfinished ...>'+b'\n'+
            br'13 access("\x62", F_OK) = -1 ENOENT (No such file or directory)'+b'\n'+
            br'12 <... stat resumed>0x1) = -1 ENOENT (No such file or directory)'+b'\n')
        events=self.module.failed_queries(raw)
        self.assertEqual([(event['pid'],event['path']) for event in events],[(13,b'b'),(12,b'a')])
        for broken in (raw.splitlines()[0]+b'\n',raw.splitlines()[-1]+b'\n',
                       raw.replace(b'<... stat resumed>',b'<... open resumed>')):
            with self.subTest(raw=broken),self.assertRaises(self.module.LookupUnavailable):
                self.module.failed_queries(broken)

    def test_writes_are_not_misrepresented_as_input_reads(self):
        raw=br'12 openat(AT_FDCWD<\x2f\x77>, "\x6f", O_WRONLY|O_CREAT, 0666) = -1 ENOENT (No such file or directory)'+b'\n'
        self.assertEqual(self.module.failed_queries(raw)[0]['intent'],'write')

    def test_truncation_unknown_formats_and_limits_refuse_observation(self):
        cases=[b'12 stat("plain", 0x1) = -1 ENOENT (missing)\n',
            br'12 stat("\x61"..., 0x1) = -1 ENOENT (missing)'+b'\n',
            br'12 newfstatat(4, "\x61", 0x1, 0) = -1 ENOENT (missing)'+b'\n',
            br'12 stat("\x00", 0x1) = -1 ENOENT (missing)'+b'\n',
            br'12 unexpected("\x61") = -1 ENOENT (missing)'+b'\n',
            b'strace: Process 12 detached\n',b'12 ???\n']
        for raw in cases:
            with self.subTest(raw=raw),self.assertRaises(self.module.LookupUnavailable):
                self.module.failed_queries(raw)
        with self.assertRaises(self.module.LookupUnavailable):
            self.module.failed_queries(b'x'*1025,max_bytes=1024)


if __name__=='__main__':unittest.main()
