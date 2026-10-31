#!/usr/bin/env python3
"""Exercise old Store -> new Store -> old Store -> new Store on synthetic data.

No provider requests or user databases. A pinned pre-phone or pre-Live source is built in
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
LIVE_BASE = "3f71452c4619503766eb78ad33b586d388457e8a"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--live", action="store_true", help="Verify durable tasks through ConversationRelay rollback and Live re-upgrade")
    parser.add_argument("--managed", action="store_true", help="Use managed build storage for local verification")
    args = parser.parse_args()
    base = LIVE_BASE if args.live else BASE
    target = "phone_live_upgrade_compat" if args.live else "phone_upgrade_compat"
    fixture = Path("crates/augmentagent-store/tests") / (target + ".rs")
    output = (args.output or ROOT / "target" / ("phone-live-upgrade" if args.live else "phone-upgrade")).resolve()
    output.mkdir(parents=True, exist_ok=True)
    receipt = {
        "kind": "synthetic-cross-version-database", "baseline": base,
        "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "worktree_dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT, text=True).strip()),
        "phases": [], "passed": False,
    }
    with tempfile.TemporaryDirectory(prefix="phone-upgrade-") as temporary:
        temporary = Path(temporary)
        old = temporary / "old"
        added = False
        try:
            if subprocess.run(["git", "cat-file", "-e", base + "^{commit}"], cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode:
                subprocess.run(["git", "fetch", "--no-tags", "origin", base], cwd=ROOT, check=True)
            subprocess.run(["git", "worktree", "add", "--detach", str(old), base], cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
            added = True
            if (old / fixture).exists():
                raise RuntimeError("baseline unexpectedly already contains upgrade fixture")
            shutil.copyfile(ROOT / fixture, old / fixture)
            env = dict(os.environ, PHONE_UPGRADE_FIXTURE_DB=str(temporary / "fixture.db"),
                       AUGMENTAGENT_GH_DISABLE="1", AUGMENTAGENT_PHONE_ENABLED="0",
                       CARGO_TERM_COLOR="never")
            for phase, cwd in [("seed", old), ("upgrade", ROOT), ("rollback", old), ("reupgrade", ROOT)]:
                print(f"Running {phase}", flush=True)
                cargo = str(ROOT / "scripts/cargo-managed.sh") if args.managed else "cargo"
                target_dir = os.environ.get("CARGO_TARGET_DIR") if args.managed else None
                target_dir = target_dir or str(ROOT / "target/phone-upgrade-build" / (base if cwd == old else "current"))
                run = subprocess.run([cargo, "test", "--locked", "-p", "augmentagent-store", "--test",
                                      target, "upgrade_fixture", "--", "--ignored", "--exact"],
                                     cwd=cwd, env=dict(env, PHONE_UPGRADE_FIXTURE_PHASE=phase,
                                         CARGO_TARGET_DIR=target_dir),
                                     text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
                (output / f"{phase}.log").write_text(run.stdout)
                passed = run.returncode == 0 and "1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out" in run.stdout
                receipt["phases"].append({"phase": phase, "source_commit": base if cwd == old else receipt["commit"], "exit_status": run.returncode, "passed": passed})
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
