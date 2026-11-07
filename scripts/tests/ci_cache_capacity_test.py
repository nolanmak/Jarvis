"""Controller capacity checks use kernel statistics, never tool error text."""
import errno
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from ci_cache_generation_test import cache


class CapacityTests(unittest.TestCase):
    def test_reserved_root_blocks_do_not_admit_a_worker_with_no_available_blocks(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'kept';path.write_bytes(b'original')
            for stats in (dict(f_bavail=0,f_bfree=100,f_favail=10),dict(f_bavail=100,f_bfree=100,f_favail=0)):
                observed=SimpleNamespace(f_frsize=4096,f_blocks=1000,f_files=1000,**stats)
                with self.subTest(stats=stats),patch.object(cache.os,'statvfs',return_value=observed):
                    with self.assertRaises(OSError) as seen:cache.require_cache_capacity(directory)
                    self.assertEqual(seen.exception.errno,errno.ENOSPC)
                self.assertEqual(path.read_bytes(),b'original')

    def test_existing_capacity_remains_usable_and_stat_errors_are_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            cache.require_cache_capacity(directory)
            with patch.object(cache.os,'statvfs',side_effect=OSError(errno.EDQUOT,'quota')):
                with self.assertRaises(OSError) as seen:cache.require_cache_capacity(directory)
                self.assertEqual(seen.exception.errno,errno.EDQUOT)


if __name__=='__main__':unittest.main()
