"""Fixture counter distinguishes Cargo queries; this does not authorize reuse."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SOURCE=Path(__file__).resolve().parents[2]/'crates/augmentagent-cli/src/ci_cargo_publication_fixture.py'
spec=importlib.util.spec_from_file_location('publication_fixture',SOURCE)
fixture=importlib.util.module_from_spec(spec);spec.loader.exec_module(fixture)


class PublicationFixtureTests(unittest.TestCase):
    def test_counter_ignores_target_queries_and_records_compilation_requests(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);fixture.prepare(root);counter=root/'counter.jsonl'
            env={**os.environ,'CI_FIXTURE_COUNTER':str(counter)}
            for arguments in (['--crate-name','___','--print=file-names'],['--crate-name','___','--print','cfg']):
                result=subprocess.run([sys.executable,str(root/'rustc-counter.py'),'/usr/bin/true',*arguments],env=env,capture_output=True,timeout=3)
                self.assertEqual(result.returncode,0)
            self.assertFalse(counter.exists())
            result=subprocess.run([sys.executable,str(root/'rustc-counter.py'),'/usr/bin/true',
                '--crate-name','generated_input_consumer','src/main.rs'],env=env,capture_output=True,timeout=3)
            self.assertEqual(result.returncode,0)
            self.assertEqual([json.loads(line) for line in counter.read_text().splitlines()],[{'crate':'generated_input_consumer'}])


if __name__=='__main__':unittest.main()
