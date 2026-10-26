#!/usr/bin/env python3
"""Explicit paid native probes, one at a time, with retained isolated evidence.

Build the test binary through cargo-managed.sh first. This launcher never builds,
changes the production daemon, or publishes to GitHub. The adoption probe uses
real Git/native models but simulated GitHub; the issue probe simulates only its
remote-main identity lookup. Provider authentication stays with the normal CLI.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile


TESTS = {
    "issue": "self_improve::native_acceptance::live_native_disposable_scope_build_verify_review_and_seed",
    "adoption": "self_improve::adoption_acceptance::live_native_source_less_adoption_repairs_reviews_and_merges",
    "adjudicator": "self_improve::native_acceptance::live_native_final_adjudicator_resolves_synthetic_contradiction",
    "writer": "self_improve::native_acceptance::live_native_writer_inventory_replay_1538",
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("probe", choices=TESTS)
    parser.add_argument("--binary", type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error("binary must be an existing executable managed test artifact")
    os.umask(0o077)
    root = Path(tempfile.mkdtemp(prefix="autoship-native-acceptance-", dir="/tmp"))
    for name in ("state", "cache", "builder", "gate"):
        (root / name).mkdir(mode=0o700)
    env = {
        "TMPDIR": "/tmp", "XDG_STATE_HOME": str(root / "state"),
        "XDG_CACHE_HOME": str(root / "cache"), "CARGO_TARGET_DIR": str(root / "builder"),
        "AUGMENTAGENT_GATE_TARGET_DIR": str(root / "gate"),
        "AUGMENTAGENT_AUTOPR_REVIEWERS": "codex",
        "AUGMENTAGENT_AUTOPR_OBSERVATION_CALL_CAP": "32",
        "AUGMENTAGENT_GATE_MIN_FREE_GB": "0", "AUGMENTAGENT_GATE_BUILD_GROWTH_GB": "0",
        "AUGMENTAGENT_RUN_RESOURCES": "1", "AUGMENTAGENT_RUN_MEMORY_BYTES": "1073741824",
        "AUGMENTAGENT_RUN_PIDS": "128", "AUGMENTAGENT_RUN_CPU_PERCENT": "100",
        "CARGO_BUILD_JOBS": "1", "CARGO_PROFILE_DEV_DEBUG": "0",
        "CARGO_PROFILE_TEST_DEBUG": "0", "CARGO_INCREMENTAL": "0", "PATH": os.environ["PATH"],
    }
    if args.probe == "issue":
        gh = root / "remote-main-fixture.py"
        gh.write_text('''#!/usr/bin/python3
import subprocess, sys
a = sys.argv[1:]
if len(a) != 4 or a[0] != 'api' or not a[1].startswith('repos/autoship-canary/full-') or not a[1].endswith('/git/ref/heads/main') or a[2:] != ['--jq', '.object.sha']:
    raise SystemExit('unsupported read-only remote-main fixture call')
subprocess.run(['/usr/bin/git', 'rev-parse', 'refs/remotes/origin/main'], check=True)
''')
        gh.chmod(0o700)
        env["GH_BIN"] = str(gh)
    unit = "augmentagent-native-" + root.name.removeprefix("autoship-native-acceptance-")
    command = ["systemd-run", "--user", "--quiet", "--wait", "--pipe", "--collect", f"--unit={unit}"]
    for value in ("Delegate=cpu memory pids", "DelegateSubgroup=daemon", "MemoryMax=2G", "TasksMax=256", "CPUQuota=200%"):
        command += ["-p", value]
    command += [f"--setenv={key}={value}" for key, value in env.items()]
    command += ["/usr/bin/env"]
    for key in ("AUGMENTAGENT_WORKFLOW_DB", "AUGMENTAGENT_AUTOPR_WORKFLOW_DB",
                "AUGMENTAGENT_TOOL_AUDIT_LOG", "AUGMENTAGENT_COOLDOWN_FILE", "AUGMENTAGENT_TOKEN_USAGE_LOG",
                "AUGMENTAGENT_AUTOPR_OPENED_FILE", "AUGMENTAGENT_AUTOPR_BASELINE_FILE",
                "AUGMENTAGENT_AUTOPR_UNREVIEWABLE_FILE", "AUGMENTAGENT_AUTOPR_FAIR_FILE",
                "AUGMENTAGENT_AUTOPR_ATTEMPTED_FILE", "AUGMENTAGENT_AUTOPR_HISTORY_FILE",
                "AUGMENTAGENT_PRUNE_TARGET_DIRS", "AUGMENTAGENT_BUILD_STORAGE_PARENT_LEASE"):
        command += ["-u", key]
    command += [str(binary), "--exact", TESTS[args.probe], "--ignored", "--nocapture"]
    digest = hashlib.sha256()
    with binary.open("rb") as artifact:
        for chunk in iter(lambda: artifact.read(1024 * 1024), b""):
            digest.update(chunk)
    print(f"Evidence: {root}", flush=True)
    with (root / "transcript.log").open("w") as log:
        result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT)
    (root / "result.json").write_text(json.dumps({"probe": args.probe, "test": TESTS[args.probe], "binary": str(binary), "binary_sha256": digest.hexdigest(), "exit_code": result.returncode, "no_production_mutation": True}, indent=2) + "\n")
    print(f"Exit: {result.returncode}; transcript: {root / 'transcript.log'}", flush=True)
    return result.returncode


if __name__ == "__main__":
    raise SystemExit(main())
