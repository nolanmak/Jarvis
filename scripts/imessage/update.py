#!/usr/bin/env python3
"""Keep this Mac's Jarvis checkout on origin/main (#2417).

The Mac-side jobs (iMessage send and owner capture, sync watchdog, Apple Notes
sync) run straight from this checkout. The Linux auto-updater never touches
it, so without this job a fix merged to main never reaches the Mac.

Each run fast-forwards to origin/main and nothing else. It never rebases,
resets or discards work: a checkout that is off main, has local edits to
tracked files, or has diverged is left alone and the reason is recorded.
When scripts/imessage changes, the long-running owner-capture job is
restarted so it loads the new code. The send job starts fresh every 15 s and
picks the change up on its own.

The outcome of every run is written to <state-dir>/mac-update.json.
"""
import argparse
import contextlib
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import time

# Jobs that stay running between launches and need a restart to load new code.
RESTART_ON_IMESSAGE_CHANGE = ("org.augmentagent.imessage-owner",)
WATCHED_PREFIX = "scripts/imessage/"
DEFAULT_REPO = Path(__file__).resolve().parents[2]
DEFAULT_STATE = Path.home() / ".local/state/augmentagent/mac-update"


def _git(repo, *args, timeout=120):
    return subprocess.run(["git", *args], cwd=repo, capture_output=True, text=True, timeout=timeout)


@contextlib.contextmanager
def lock(state_dir):
    state_dir.mkdir(parents=True, exist_ok=True)
    with open(state_dir / "mac-update.lock", "w") as fh:
        try:
            fcntl.flock(fh, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            yield False
            return
        yield True


def launchd_restart(label):
    service = f"gui/{os.getuid()}/{label}"
    if subprocess.run(["launchctl", "print", service], capture_output=True).returncode == 0:
        subprocess.run(["launchctl", "kickstart", "-k", service], capture_output=True)


def _update(repo, restart):
    branch = _git(repo, "rev-parse", "--abbrev-ref", "HEAD").stdout.strip()
    if branch != "main":
        return "skipped: not on main"
    fetched = _git(repo, "fetch", "--quiet", "origin", "main")
    if fetched.returncode != 0:
        return f"failed: fetch: {fetched.stderr.strip()[:200]}"
    head = _git(repo, "rev-parse", "HEAD").stdout.strip()
    target = _git(repo, "rev-parse", "origin/main").stdout.strip()
    if head == target:
        return "up-to-date"
    if _git(repo, "merge-base", "--is-ancestor", "HEAD", "origin/main").returncode != 0:
        return "skipped: diverged from origin/main"
    if _git(repo, "status", "--porcelain", "--untracked-files=no").stdout.strip():
        return "skipped: local changes"
    changed = _git(repo, "diff", "--name-only", head, target).stdout.split()
    merged = _git(repo, "merge", "--ff-only", "--quiet", "origin/main")
    if merged.returncode != 0:
        return f"failed: merge: {merged.stderr.strip()[:200]}"
    if any(p.startswith(WATCHED_PREFIX) for p in changed):
        for label in RESTART_ON_IMESSAGE_CHANGE:
            restart(label)
    return "updated"


def run(repo, state_dir, restart=launchd_restart):
    repo, state_dir = Path(repo), Path(state_dir)
    with lock(state_dir) as got:
        if not got:
            return "skipped: another update is running"
        try:
            result = _update(repo, restart)
        except subprocess.TimeoutExpired as e:
            result = f"failed: timeout: {' '.join(e.cmd[:3])}"
        commit = _git(repo, "rev-parse", "HEAD").stdout.strip()
        stamp = {"result": result, "commit": commit, "at_ms": int(time.time() * 1000)}
        tmp = state_dir / "mac-update.json.tmp"
        tmp.write_text(json.dumps(stamp) + "\n")
        tmp.replace(state_dir / "mac-update.json")
    return result


def parser():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--repo", type=Path, default=DEFAULT_REPO)
    ap.add_argument("--state-dir", type=Path, default=DEFAULT_STATE)
    return ap


def main(argv=None):
    args = parser().parse_args(argv)
    result = run(args.repo, args.state_dir)
    print(f"{time.strftime('%Y-%m-%dT%H:%M:%S%z')} {result}", flush=True)
    return 1 if result.startswith("failed") else 0


if __name__ == "__main__":
    sys.exit(main())
