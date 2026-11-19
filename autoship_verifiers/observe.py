import contextlib
import importlib.util
import io
import json
import os
import sys
import tempfile
from pathlib import Path
from types import SimpleNamespace

root = Path.cwd()
sys.path.insert(0, str(root))
path = root / "scripts" / "phone-offline-tests.py"
spec = importlib.util.spec_from_file_location("phone_offline", path)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

secrets = ("GITHUB_TOKEN", "OPENAI_API_KEY", "TWILIO_AUTH_TOKEN")
original_env = {name: os.environ.get(name) for name in secrets}
original_argv = sys.argv[:]
original_run = module.subprocess.run
original_check_output = module.subprocess.check_output

def summary():
    return "test result: ok. 999 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.1s\n"

def observe_case(fail):
    calls = []
    def check_output(*args, **kwargs):
        return "test-commit\n"
    def run(command, **kwargs):
        env = kwargs["env"]
        calls.append([env.get("AUGMENTAGENT_GH_DISABLE"), env.get("AUGMENTAGENT_PHONE_ENABLED"), [name for name in secrets if name in env]])
        if fail:
            raise FileNotFoundError("cargo unavailable")
        return SimpleNamespace(returncode=0, stdout=summary())
    module.subprocess.check_output = check_output
    module.subprocess.run = run
    with tempfile.TemporaryDirectory() as directory:
        output = Path(directory) / "phone-offline"
        sys.argv = [str(path), "--output", str(output)]
        stderr = io.StringIO()
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(stderr):
            try:
                result = module.main()
                outcome = "returned:" + str(result)
            except Exception as error:
                outcome = "raised:" + type(error).__name__
        receipt_path = output / "receipt.json"
        receipt = json.loads(receipt_path.read_text()) if receipt_path.exists() else {"passed": None, "suites": []}
    return outcome, stderr.getvalue(), calls, receipt

try:
    for name in secrets:
        os.environ[name] = "synthetic-credential"
    success = observe_case(False)
    failed = observe_case(True)
finally:
    module.subprocess.run = original_run
    module.subprocess.check_output = original_check_output
    sys.argv = original_argv
    for name, value in original_env.items():
        if value is None:
            os.environ.pop(name, None)
        else:
            os.environ[name] = value

good = "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.1s"
def rejection(output, code, **kwargs):
    try:
        module.validate_output(output, code, 3, **kwargs)
    except Exception as error:
        return type(error).__name__
    return "accepted"

success_outcome, _, success_calls, success_receipt = success
failed_outcome, failed_stderr, _, failed_receipt = failed
failed_item = failed_receipt.get("suites", [{}])[0] if failed_receipt.get("suites") else {}
observed = [
    [success_outcome, len(success_calls), sorted({tuple([row[0], row[1], tuple(row[2])]) for row in success_calls}), success_receipt.get("passed"), len(success_receipt.get("suites", []))],
    [failed_outcome, failed_stderr, failed_receipt.get("passed"), len(failed_receipt.get("suites", [])), [failed_item.get("suite"), failed_item.get("exit_status"), failed_item.get("passed"), failed_item.get("error")]],
    [rejection("", 0), rejection(good.replace("0 ignored", "1 ignored"), 0), rejection(good.replace("0 filtered out", "1 filtered out"), 0), rejection(good + "\n" + good, 0)],
]
print(json.dumps(observed))
