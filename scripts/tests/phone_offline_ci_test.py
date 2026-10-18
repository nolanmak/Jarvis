"""The phone CI receipt must reject missing, filtered and skipped coverage."""
import importlib.util
from pathlib import Path
import unittest

PATH = Path(__file__).resolve().parents[1] / "phone-offline-tests.py"

class PhoneOfflineCiTest(unittest.TestCase):
    def test_receipt_rejects_incomplete_test_runs(self):
        spec = importlib.util.spec_from_file_location("phone_offline", PATH)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        good = "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.1s"
        self.assertEqual(module.validate_output(good, 0, 3), 3)
        for output, code in [("", 0), (good, 1), (good.replace("3 passed", "0 passed"), 0), (good.replace("0 ignored", "1 ignored"), 0), (good.replace("0 filtered out", "1 filtered out"), 0), (good.replace("3 passed", "2 passed"), 0), (good + "\n" + good, 0)]:
            with self.subTest(output=output, code=code):
                with self.assertRaises(ValueError):
                    module.validate_output(output, code, 3)

    def test_named_cli_target_can_filter_unrelated_tests_but_cannot_skip_required_case(self):
        spec = importlib.util.spec_from_file_location("phone_offline", PATH)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        good = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 2000 filtered out; finished in 0.1s"
        self.assertEqual(module.validate_output(good, 0, 1, allow_filtered=True), 1)
        for bad in [good.replace("1 passed", "0 passed"), good.replace("0 ignored", "1 ignored")]:
            with self.assertRaises(ValueError):
                module.validate_output(bad, 0, 1, allow_filtered=True)
