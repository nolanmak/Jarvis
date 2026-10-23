#!/usr/bin/env python3
"""Exercise old Store -> new Store -> old Store -> new Store on synthetic data.

No provider requests or user databases. The pinned pre-phone source is built in
an isolated Git worktree; this is database compatibility, not installer rollback.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
BASE = "4caf3942414a3f499fcd2b4be539ee788cea1a65"
FIXTURE = Path("crates/augmentagent-store/tests/phone_upgrade_compat.rs")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "target/phone-upgrade")
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    receipt = {
        "kind": "synthetic-cross-version-database", "baseline": BASE,
        "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "worktree_dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT, text=True).strip()),
        "phases": [], "passed": False,
    }
    with tempfile.TemporaryDirectory(prefix="phone-upgrade-") as temporary:
        temporary = Path(temporary)
        old = temporary / "old"
        added = False
        try:
            if subprocess.run(["git", "cat-file", "-e", BASE + "^{commit}"], cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode:
                subprocess.run(["git", "fetch", "--no-tags", "origin", BASE], cwd=ROOT, check=True)
            subprocess.run(["git", "worktree", "add", "--detach", str(old), BASE], cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
            added = True
            if (old / FIXTURE).exists():
                raise RuntimeError("baseline unexpectedly already contains upgrade fixture")
            shutil.copyfile(ROOT / FIXTURE, old / FIXTURE)
            env = dict(os.environ, PHONE_UPGRADE_FIXTURE_DB=str(temporary / "fixture.db"),
                       AUGMENTAGENT_GH_DISABLE="1", AUGMENTAGENT_PHONE_ENABLED="0",
                       CARGO_TERM_COLOR="never")
            for phase, cwd in [("seed", old), ("upgrade", ROOT), ("rollback", old), ("reupgrade", ROOT)]:
                print(f"Running {phase}", flush=True)
                run = subprocess.run(["cargo", "test", "--locked", "-p", "augmentagent-store", "--test",
                                      "phone_upgrade_compat", "upgrade_fixture", "--", "--ignored", "--exact"],
                                     cwd=cwd, env=dict(env, PHONE_UPGRADE_FIXTURE_PHASE=phase,
                                         CARGO_TARGET_DIR=str(ROOT / "target/phone-upgrade-build" / (BASE if cwd == old else "current"))),
                                     text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
                (output / f"{phase}.log").write_text(run.stdout)
                passed = run.returncode == 0 and "1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out" in run.stdout
                receipt["phases"].append({"phase": phase, "source_commit": BASE if cwd == old else receipt["commit"], "exit_status": run.returncode, "passed": passed})
                if not passed:
                    raise RuntimeError(f"{phase} failed; inspect {output / (phase + '.log')}")
            receipt["passed"] = True
        finally:
            (output / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
            if added:
                subprocess.run(["git", "worktree", "remove", "--force", str(old)], cwd=ROOT, check=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
