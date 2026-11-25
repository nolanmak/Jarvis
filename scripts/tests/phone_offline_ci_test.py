"""The phone CI receipt must reject missing, filtered and skipped coverage."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

PATH = Path(__file__).resolve().parents[1] / "phone-offline-tests.py"
SUMMARY = "test result: ok. 999 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.1s\n"

def load():
    spec = importlib.util.spec_from_file_location("phone_offline", PATH)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

class PhoneOfflineCiTest(unittest.TestCase):
    def run_suites(self, module, output, run):
        with mock.patch.object(sys, "argv", [str(PATH), "--output", str(output)]), \
                mock.patch.object(module.subprocess, "check_output", return_value="synthetic-commit\n"), \
                mock.patch.object(module.subprocess, "run", side_effect=run) as calls, \
                contextlib.redirect_stdout(io.StringIO()), \
                contextlib.redirect_stderr(io.StringIO()) as errors:
            status = module.main()
        return status, calls, errors.getvalue(), json.loads((output / "receipt.json").read_text())

    def test_receipt_rejects_incomplete_test_runs(self):
        module = load()
        good = "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.1s"
        self.assertEqual(module.validate_output(good, 0, 3), 3)
        for output, code in [("", 0), (good, 1), (good.replace("3 passed", "0 passed"), 0), (good.replace("0 ignored", "1 ignored"), 0), (good.replace("0 filtered out", "1 filtered out"), 0), (good.replace("3 passed", "2 passed"), 0), (good + "\n" + good, 0)]:
            with self.subTest(output=output, code=code):
                with self.assertRaises(ValueError):
                    module.validate_output(output, code, 3)

    def test_named_cli_target_can_filter_unrelated_tests_but_cannot_skip_required_case(self):
        module = load()
        good = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 2000 filtered out; finished in 0.1s"
        self.assertEqual(module.validate_output(good, 0, 1, allow_filtered=True), 1)
        for bad in [good.replace("1 passed", "0 passed"), good.replace("0 ignored", "1 ignored")]:
            with self.assertRaises(ValueError):
                module.validate_output(bad, 0, 1, allow_filtered=True)

    def test_suite_environment_disables_phone_integration_and_drops_credentials(self):
        module = load()
        supplied = {"PATH": "/usr/bin", "HOME": "/home/synthetic", "CARGO_PROFILE_TEST_DEBUG": "0",
                    "GITHUB_TOKEN": "synthetic", "OPENAI_API_KEY": "synthetic",
                    "TWILIO_AUTH_TOKEN": "synthetic", "TWILIO_ACCOUNT_SID": "synthetic",
                    "AUGMENTAGENT_PHONE_VERIFICATION_KEY": "synthetic",
                    "AUGMENTAGENT_SLACK_SIGNING_SECRET": "synthetic"}
        self.assertEqual(module.suite_environment(supplied),
                         {"PATH": "/usr/bin", "HOME": "/home/synthetic", "CARGO_PROFILE_TEST_DEBUG": "0",
                          "AUGMENTAGENT_GH_DISABLE": "1", "AUGMENTAGENT_PHONE_ENABLED": "0",
                          "CARGO_TERM_COLOR": "never"})

    def test_every_required_suite_runs_without_credentials_and_is_recorded(self):
        module = load()
        with tempfile.TemporaryDirectory() as directory, \
                mock.patch.dict(module.os.environ, {"GITHUB_TOKEN": "synthetic"}):
            output = Path(directory) / "phone-offline"
            status, calls, errors, receipt = self.run_suites(
                module, output, lambda *_, **__: SimpleNamespace(returncode=0, stdout=SUMMARY))
        self.assertEqual((status, errors), (0, ""))
        self.assertTrue(receipt["passed"])
        self.assertEqual(len(receipt["suites"]), calls.call_count)
        self.assertTrue(all(item["passed"] for item in receipt["suites"]))
        for call in calls.call_args_list:
            environment = call.kwargs["env"]
            self.assertNotIn("GITHUB_TOKEN", environment)
            self.assertEqual(environment["AUGMENTAGENT_GH_DISABLE"], "1")
            self.assertEqual(environment["AUGMENTAGENT_PHONE_ENABLED"], "0")

    def test_unrunnable_required_target_fails_with_a_local_error_and_is_recorded(self):
        module = load()
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "phone-offline"
            status, calls, errors, receipt = self.run_suites(
                module, output, FileNotFoundError("cargo unavailable"))
        self.assertEqual(status, 1)
        self.assertEqual(calls.call_count, 1)
        self.assertEqual(errors, "augmentagent-docs-private_attachments: required target could not complete: cargo unavailable\n")
        self.assertFalse(receipt["passed"])
        self.assertEqual([(item["suite"], item["exit_status"], item["passed"], item["error"]) for item in receipt["suites"]],
                         [("augmentagent-docs-private_attachments", None, False, "cargo unavailable")])
