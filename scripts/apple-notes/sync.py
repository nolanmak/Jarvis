#!/usr/bin/env python3
"""Export Apple Notes to a private bundle; optionally commit and push it.

    python3 scripts/apple-notes/sync.py --out ~/AppleNotesSync            # write bundle
    python3 scripts/apple-notes/sync.py --out ~/AppleNotesSync --commit   # ...then git add/commit/push

Unlike the iMessage exporter, the output directory is expected to be a git
repository: notes are mutable documents, and git history is how edits are
kept. Output inside this source checkout is still rejected. Every body is
scrubbed on the way in and the whole bundle is re-checked before a commit.
"""
import argparse
import fcntl
import json
import os
import shutil
import sqlite3
import subprocess
import sys
from datetime import datetime
from pathlib import Path

from apple_notes_sync import sync
from scrub import check_bundle

SOURCE_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_DB = Path.home() / "Library/Group Containers/group.com.apple.notes/NoteStore.sqlite"
DEFAULT_OUT = Path.home() / ".local/share/augmentagent/apple-notes-bundle"


def output_path(value):
    path = Path(value).expanduser().resolve()
    if path == SOURCE_ROOT or SOURCE_ROOT in path.parents:
        raise argparse.ArgumentTypeError("notes output must be outside the source checkout")
    return path


def parser():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--db", type=lambda p: Path(p).expanduser().resolve(), default=DEFAULT_DB)
    ap.add_argument("--out", type=output_path, default=str(DEFAULT_OUT))
    ap.add_argument("--commit", action="store_true", help="git add/commit/push the bundle after syncing")
    ap.add_argument("--s3-bucket", help="opt in to uploading attachments to your private S3 bucket")
    ap.add_argument("--aws-profile")
    ap.add_argument("--media-root", type=lambda p: Path(p).expanduser().resolve(),
                    help="Group Container holding Accounts/<account>/Media (default: the --db directory)")
    return ap


def load_config(out):
    """Optional `<out>/config.json`: {"skip_folders": [...], "skip_notes": [uuid, ...]}."""
    path = out / "config.json"
    return json.loads(path.read_text()) if path.exists() else {}


def git(out, *args, **kw):
    return subprocess.run(["git", "-C", str(out), *args], check=True, capture_output=True, text=True, **kw)


def commit(out, counts, titles):
    if not (out / ".git").exists():
        raise RuntimeError(f"{out} is not a git repository; init it (or drop --commit)")
    hits = list(check_bundle(out))
    if hits:
        for path, line, kind in hits:
            print(f"{path}:{line} {kind}", file=sys.stderr)
        raise RuntimeError("secret scrubber check failed; refusing to commit (see findings above)")
    if not git(out, "status", "--porcelain").stdout.strip():
        print("nothing to commit")
        return
    stamp = datetime.now().strftime("%Y-%m-%d %H:%M")
    subject = (f"Sync notes {stamp}: {counts['new']} new, {counts['updated']} updated, "
               f"{counts['renamed']} renamed, {counts['deleted']} deleted")
    body = "\n".join(f"- {t}" for t in titles)
    git(out, "add", "-A")
    git(out, "commit", "-q", "-m", subject, "-m", body)
    if not git(out, "remote").stdout.strip():
        print(f"committed (no remote configured): {subject}")
        return
    try:
        git(out, "push", "-q", "origin", "HEAD")  # no upstream needed on a fresh clone
    except subprocess.CalledProcessError as e:
        print(f"committed but push failed (will retry next run): {e.stderr.strip()}", file=sys.stderr)
        return
    print(f"committed and pushed: {subject}")


def uploader(bucket, profile):
    """`aws s3 cp` closure matching apple_notes_sync's `s3['uploader']` contract (#1061)."""
    def upload(path, bucket_name, key):
        cmd = ["aws", "s3", "cp", path, f"s3://{bucket_name}/{key}", "--only-show-errors"]
        if profile:
            cmd += ["--profile", profile]
        try:
            return subprocess.run(cmd, capture_output=True, timeout=300).returncode == 0
        except subprocess.TimeoutExpired:
            return False
    return {"bucket": bucket, "uploader": upload}


def run(args):
    os.umask(0o077)
    if args.s3_bucket and not shutil.which("aws"):
        raise RuntimeError("AWS CLI is required for --s3-bucket")
    args.out.mkdir(parents=True, exist_ok=True, mode=0o700)
    with (args.out / ".sync.lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            print("another sync is running; skipped")
            return
        config = load_config(args.out)
        touched = []
        counts = sync(args.db, args.out, args.out / ".sync_state.json", config=config, touched=touched,
                      s3=uploader(args.s3_bucket, args.aws_profile) if args.s3_bucket else None,
                      media_root=args.media_root)
        print("synced: " + ", ".join(f"{counts[k]} {k}" for k in ("new", "updated", "renamed", "deleted", "unchanged", "skipped")))
        if args.commit:
            commit(args.out, counts, touched)


def main(argv=None):
    args = parser().parse_args(argv)
    try:
        run(args)
    except sqlite3.Error:
        print("Cannot read the Notes database. Check --db and macOS Full Disk Access; see docs/APPLE-NOTES.md.", file=sys.stderr)
        return 1
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"sync failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
