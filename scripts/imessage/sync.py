#!/usr/bin/env python3
"""Export Messages to a private bundle; optionally mirror it to an agent over SSH."""
import argparse
import fcntl
import glob
import os
from pathlib import Path
import re
import shutil
import sqlite3
import subprocess
import sys

from imessage_sync import load_contacts, sync

SOURCE_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_DB = Path.home() / "Library/Messages/chat.db"
DEFAULT_OUT = Path.home() / ".local/share/augmentagent/imessage-bundle"


def output_path(value):
    path = Path(value).expanduser().resolve()
    if path == SOURCE_ROOT or SOURCE_ROOT in path.parents:
        raise argparse.ArgumentTypeError("message output must be outside the source checkout")
    # Also reject other Git checkouts, including worktrees, to avoid publishing data.
    if any((parent / ".git").exists() for parent in (path, *path.parents)):
        raise argparse.ArgumentTypeError("message output must be outside Git repositories")
    return path


def remote_path(value):
    # A deliberately small SSH destination grammar; no shell metacharacters or options.
    if not re.fullmatch(r"(?:[A-Za-z0-9_][A-Za-z0-9_.-]*@)?[A-Za-z0-9][A-Za-z0-9_.-]*:/[A-Za-z0-9_./-]+", value):
        raise argparse.ArgumentTypeError("use [user@]host:/absolute/private/bundle/path (no spaces)")
    path = value.split(":", 1)[1]
    if path == "/" or ".." in path.split("/"):
        raise argparse.ArgumentTypeError("use a dedicated private bundle directory")
    return value.rstrip("/") + "/"


def parser():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--db", type=lambda p: Path(p).expanduser().resolve(), default=DEFAULT_DB)
    ap.add_argument("--out", type=output_path, default=str(DEFAULT_OUT))
    ap.add_argument("--remote", type=remote_path, help="optional SSH destination; requires rsync on both hosts")
    ap.add_argument("--no-contacts", action="store_true")
    ap.add_argument("--s3-bucket", help="opt in to uploading attachments to your private S3 bucket")
    ap.add_argument("--aws-profile")
    return ap


def mirror(out, remote):
    # No --delete: never remove files on the receiving host. The index determines
    # which conversations the daemon reads. Do not transfer cursor/local paths.
    subprocess.run([
        "rsync", "-rt", "--delay-updates", "--chmod=D700,F600",
        "-e", "ssh -o BatchMode=yes -o ConnectTimeout=15",
        str(out / "conversations"), str(out / "index.md"), remote,
    ], check=True, timeout=1800)


def run(args):
    os.umask(0o077)
    if args.remote and not shutil.which("rsync"):
        raise RuntimeError("rsync is required for --remote")
    if args.s3_bucket and not shutil.which("aws"):
        raise RuntimeError("AWS CLI is required for --s3-bucket")
    args.out.mkdir(parents=True, exist_ok=True, mode=0o700)
    with (args.out / ".sync.lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            print("another sync is running; skipped")
            return
        contacts = None
        if not args.no_contacts:
            contacts = load_contacts(glob.glob(str(
                Path.home() / "Library/Application Support/AddressBook/**/*.abcddb"
            ), recursive=True))
        s3 = None
        if args.s3_bucket:
            def upload(path, bucket, key):
                cmd = ["aws", "s3", "cp", path, f"s3://{bucket}/{key}", "--only-show-errors"]
                if args.aws_profile:
                    cmd += ["--profile", args.aws_profile]
                try:
                    return subprocess.run(cmd, capture_output=True, timeout=300).returncode == 0
                except subprocess.TimeoutExpired:
                    return False
            s3 = {"bucket": args.s3_bucket, "uploader": upload}
        result = sync(args.db, args.out, args.out / ".sync_state.json", contacts=contacts, s3=s3)
        print(f"synced {result['messages']} messages across {result['conversations']} conversations")
        # Retry transport even when there are no new messages since the last export.
        if args.remote:
            mirror(args.out, args.remote)


def main(argv=None):
    args = parser().parse_args(argv)
    try:
        run(args)
    except sqlite3.Error:
        print("Cannot read Messages database. Check --db and macOS Full Disk Access; see docs/IMESSAGE.md.", file=sys.stderr)
        return 1
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"sync failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
