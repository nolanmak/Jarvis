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
    ("augmentagent-docs", "--test", "private_attachments", 2),
    ("augmentagent-phone", "--lib", None, 11),
    ("augmentagent-phone", "--test", "relay", 6),
    ("augmentagent-phone", "--test", "service", 37),
    ("augmentagent-phone", "--test", "metrics", 3),
    ("augmentagent-phone", "--test", "recovery", 7),
    ("augmentagent-phone", "--test", "mcp", 2),
    ("augmentagent-phone", "--test", "config", 2),
    ("augmentagent-phone", "--test", "runtime", 3),
    ("augmentagent-store", "--test", "phone_calls", 17),
    ("augmentagent-store", "--test", "phone_migration", 1),
    ("augmentagent-store", "--test", "phone_turn_metrics", 3),
    ("augmentagent-store", "--test", "local_chat", 3),
    ("augmentagent-store", "--test", "surface_owner", 18),
    ("augmentagent-store", "--test", "surface_delivery", 36),
    ("augmentagent-approval-discord", "--lib", None, 255),
    ("augmentagent-approval-discord", "--test", "phone_boundary", 2),
    ("augmentagent-approval-discord", "--test", "phone_delivery", 3),
    ("augmentagent-channel-core", "--test", "phone_agent", 3),
    ("augmentagent-channel-core", "--test", "capability_inventory", 9),
    ("augmentagent-channel-core", "--test", "surface_turn", 10),
    ("augmentagent-channel-slack", "--test", "interactive_surface", 20),
    ("augmentagent-channel-slack", "--test", "conversation_harness", 11),
    ("augmentagent-channel-slack", "--test", "conversation_history", 5),
    ("augmentagent-channel-slack", "--test", "phone_privacy", 1),
    ("augmentagent-channel-slack", "--test", "ingest_reconcile", 11),
    ("augmentagent-channel-slack", "--test", "ingest_catch_up", 5),
    ("augmentagent-channel-slack", "--test", "owner_auth", 31),
    ("augmentagent-channel-slack", "--test", "phone_delivery", 1),
    ("augmentagent-channel-whatsapp", "--lib", None, 44),
    ("augmentagent-channel-whatsapp", "--test", "phone_privacy", 1),
    ("augmentagent-channel-whatsapp", "--test", "self_chat", 1),
    ("augmentagent-channel-whatsapp", "--test", "phone_commands", 1),
    ("augmentagent-channel-whatsapp", "--test", "phone_delivery", 5),
    ("augmentagent-channel-whatsapp-history", "--test", "history", 10),
]
SUMMARY = re.compile(r"test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out;")

def validate_output(output, returncode, minimum, *, allow_filtered=False):
    summaries = SUMMARY.findall(output)
    if returncode != 0 or len(summaries) != 1:
        raise ValueError("required target failed or has no unique test summary")
    status, passed, failed, ignored, measured, filtered = summaries[0]
    if status != "ok" or int(passed) < minimum or any(int(n) for n in (failed, ignored, measured)) or (int(filtered) and not allow_filtered):
        raise ValueError("required target has missing, failed, skipped or filtered cases")
    return int(passed)

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "target/phone-offline")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, AUGMENTAGENT_GH_DISABLE="1", AUGMENTAGENT_PHONE_ENABLED="0", CARGO_TERM_COLOR="never")
    receipt = {"kind":"offline-only", "commit":subprocess.check_output(["git","rev-parse","HEAD"],cwd=ROOT,text=True).strip(), "worktree_dirty":bool(subprocess.check_output(["git","status","--porcelain"],cwd=ROOT,text=True).strip()), "os":platform.system(), "architecture":platform.machine(), "suites":[], "passed":False}
    jobs = []
    for package, selector, target, minimum in SUITES:
        label = package + "-" + (target or "lib")
        command = ["cargo", "test", "--locked", "-p", package, selector] + ([target] if target else [])
        jobs.append((label, command, minimum, False))
    # These required CLI modules share a binary with unrelated, environment-
    # dependent tests. Only unrelated tests may be filtered; zero/ignored cases
    # or fewer than the required number still fail.
    for module, minimum in [("wiki_question_stdin_tests::", 2), ("local_phone::tests::", 1), ("phone_metrics::tests::", 2), ("local_phone_transport::tests::", 5), ("notify_tests::", 17), ("phone_runtime::tests::", 4), ("surface_conformance_tests::", 2), ("discord_voice_session_tests::", 6), ("discord_phone_session_tests::", 1), ("whatsapp_phone_session_tests::", 1), ("whatsapp_phone::tests::", 3), ("discord_phone::tests::", 2), ("discord_phone::installation_tests::", 1)]:
        jobs.append(("augmentagent-cli-" + module.rstrip(":").replace("::", "-"),
                     ["cargo", "test", "--locked", "-p", "augmentagent-cli", "--bin", "augmentagent", module, "--", "--test-threads=1"], minimum, True))
    # Captured iMessage/Notes files remain readable across the voice migration.
    jobs.append(("augmentagent-channel-journal-s3",
                 ["cargo", "test", "--locked", "-p", "augmentagent-channel-journal", "--lib",
                  "s3::tests::", "--", "--test-threads=1"], 7, True))
    # This library also contains explicitly ignored live-provider qualification
    # tests. Require the deterministic privacy regression by its exact name.
    jobs.append(("augmentagent-channel-discord-dm-phone-privacy",
                 ["cargo", "test", "--locked", "-p", "augmentagent-channel-discord-dm", "--lib",
                  "channel::tests::private_phone_setup_is_not_stored_or_sent_to_reasoner", "--", "--exact"], 1, True))
    try:
        for label, command, minimum, allow_filtered in jobs:
            print("Running " + label, flush=True)
            run = subprocess.run(command, cwd=ROOT, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            (args.output / (label + ".log")).write_text(run.stdout)
            item = {"suite":label,"command":command,"exit_status":run.returncode,"passed":False}
            receipt["suites"].append(item)
            try:
                item["tests"] = validate_output(run.stdout, run.returncode, minimum, allow_filtered=allow_filtered)
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
