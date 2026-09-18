"""Private, atomic installer writes; no external services."""
import importlib.util
import os
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('model_router_installer', Path(__file__).resolve().parents[1] / 'install-model-router.py')
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)


class PrivateConfiguration(unittest.TestCase):
    def test_replace_is_private_and_does_not_follow_an_existing_symlink(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            original = root / 'unrelated'
            original.write_text('leave me alone')
            config = root / 'config'
            config.symlink_to(original)
            installer.private_write(config, 'new secret')
            self.assertEqual(original.read_text(), 'leave me alone')
            self.assertFalse(config.is_symlink())
            self.assertEqual(config.read_text(), 'new secret')
            self.assertEqual(os.stat(config).st_mode & 0o777, 0o600)
            installer.private_write(config, 'replacement secret')
            self.assertEqual(config.read_text(), 'replacement secret')
            self.assertEqual(len(list(root.iterdir())), 2)


if __name__ == '__main__':
    unittest.main()
