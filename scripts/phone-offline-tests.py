#!/usr/bin/env python3
"""Run the required #1496 offline suites, rejecting absent/empty/skipped cases.

No credentials or live provider calls. This receipt is not live qualification.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
# Each invocation must run exactly one target, with no name filters. Minimum
# counts prevent silent deletion of the current required coverage.
SUITES = [
    ("augmentagent-phone", "--lib", None, 4),
    ("augmentagent-phone", "--test", "relay", 3),
    ("augmentagent-phone", "--test", "service", 13),
    ("augmentagent-phone", "--test", "mcp", 2),
    ("augmentagent-phone", "--test", "config", 1),
    ("augmentagent-phone", "--test", "runtime", 1),
    ("augmentagent-store", "--test", "phone_calls", 11),
    ("augmentagent-channel-core", "--test", "phone_agent", 2),
    ("augmentagent-channel-core", "--test", "surface_turn", 8),
]
SUMMARY = re.compile(r"test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out;")

def validate_output(output, returncode, minimum):
    summaries = SUMMARY.findall(output)
    if returncode != 0 or len(summaries) != 1:
        raise ValueError("required target failed or has no unique test summary")
    status, passed, failed, ignored, measured, filtered = summaries[0]
    if status != "ok" or int(passed) < minimum or any(int(n) for n in (failed, ignored, measured, filtered)):
        raise ValueError("required target has missing, failed, skipped or filtered cases")
    return int(passed)

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "target/phone-offline")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, AUGMENTAGENT_GH_DISABLE="1", AUGMENTAGENT_PHONE_ENABLED="0", CARGO_TERM_COLOR="never")
    receipt = {"kind":"offline-only", "commit":subprocess.check_output(["git","rev-parse","HEAD"],cwd=ROOT,text=True).strip(), "os":platform.system(), "architecture":platform.machine(), "suites":[], "passed":False}
    try:
        for package, selector, target, minimum in SUITES:
            label = package + "-" + (target or "lib")
            command = ["cargo","test","--locked","-p",package,selector] + ([target] if target else [])
            print("Running " + label, flush=True)
            run = subprocess.run(command, cwd=ROOT, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            (args.output / (label + ".log")).write_text(run.stdout)
            item = {"suite":label,"command":command,"exit_status":run.returncode,"passed":False}
            receipt["suites"].append(item)
            try:
                item["tests"] = validate_output(run.stdout, run.returncode, minimum)
                item["passed"] = True
            except ValueError as error:
                print(run.stdout, file=sys.stderr)
                raise ValueError(label + ": " + str(error)) from error
        receipt["passed"] = True
        return 0
    except ValueError as error:
        print(str(error), file=sys.stderr)
        return 1
    finally:
        (args.output / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")

if __name__ == "__main__":
    raise SystemExit(main())
