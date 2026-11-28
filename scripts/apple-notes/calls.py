#!/usr/bin/env python3
"""Export iPhone call recordings from Apple Notes as meeting transcripts.

    python3 scripts/apple-notes/calls.py --out ~/ShipSystemTranscripts
    python3 scripts/apple-notes/calls.py --out ~/ShipSystemTranscripts --summarize --commit

`--out` is a clone of the FlyOnTheWall transcript repository. Each call is
written to `meetings/` in FlyOnTheWall's meeting format, so the agent's
`fotw` channel ingests it like a recorded meeting. Only files this exporter
wrote are ever added or changed; FlyOnTheWall's own files are left alone.
See docs/APPLE-NOTES.md ("Call recordings").
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

from call_recordings import export_calls
from sync import DEFAULT_DB, output_path

DEFAULT_STATE = Path.home() / ".local/share/augmentagent/apple-call-recordings.json"

SUMMARY_PROMPT = """You are summarizing a recorded phone call for the caller's own notes.
The transcript is on stdin, in a markdown file. Speaker "Me" is the caller.
Reply with JSON only, no prose and no code fence:
{"summary": "<3-6 sentences: who the call was with, what was discussed, what was decided>",
 "action_items": ["<concrete follow-up, with an owner when one was said>", ...]}
Use only what the transcript says. If it is too short or garbled to summarize, say so in
"summary" and return an empty "action_items" list."""


def parser():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--db", type=lambda p: Path(p).expanduser().resolve(), default=DEFAULT_DB)
    ap.add_argument("--out", type=output_path, required=True,
                    help="clone of the transcript repository (meetings/ is written inside it)")
    ap.add_argument("--state", type=lambda p: Path(p).expanduser().resolve(), default=DEFAULT_STATE)
    ap.add_argument("--summarize", action="store_true",
                    help="write a summary and action items with `claude -p` when Apple has not")
    ap.add_argument("--model", default="sonnet", help="model for --summarize (default: sonnet)")
    ap.add_argument("--settle-minutes", type=int, default=5,
                    help="wait this long after the note last changed before exporting")
    ap.add_argument("--commit", action="store_true", help="git commit and push the new transcripts")
    return ap


def claude_summarizer(model):
    """`summarizer` for export_calls, backed by the Claude Code CLI."""
    exe = shutil.which("claude")
    if not exe:
        raise RuntimeError("--summarize needs the `claude` CLI on PATH")

    def summarize(markdown):
        done = subprocess.run(
            [exe, "-p", SUMMARY_PROMPT, "--model", model, "--output-format", "text"],
            input=markdown, capture_output=True, text=True, timeout=300,
        )
        if done.returncode != 0:
            raise RuntimeError(f"claude exited {done.returncode}: {done.stderr.strip()[:200]}")
        raw = done.stdout.strip()
        start, end = raw.find("{"), raw.rfind("}")
        if start < 0 or end < start:
            raise RuntimeError("summarizer reply had no JSON object")
        reply = json.loads(raw[start:end + 1])
        summary = str(reply.get("summary") or "").strip()
        items = [str(a).strip() for a in reply.get("action_items") or [] if str(a).strip()]
        return summary, items

    return summarize


def git(out, *args):
    return subprocess.run(["git", "-C", str(out), *args], check=True, capture_output=True, text=True)


def commit(out, written):
    """Commit exactly the files this run wrote, then push. A rejected push
    merges the remote (never rebases) and tries once more; FlyOnTheWall
    writes other files in the same repository through the GitHub API."""
    if not (out / ".git").exists():
        raise RuntimeError(f"{out} is not a git repository; clone the transcript repo there (or drop --commit)")
    if not written:
        print("nothing to commit")
        return
    has_remote = bool(git(out, "remote").stdout.strip())
    if has_remote:
        try:
            git(out, "pull", "-q", "--no-rebase", "--no-edit", "origin", "HEAD")
        except subprocess.CalledProcessError as e:
            print(f"pull failed (committing locally anyway): {e.stderr.strip()}", file=sys.stderr)
    git(out, "add", "--", *written)
    if not git(out, "diff", "--cached", "--name-only").stdout.strip():
        print("nothing to commit")
        return
    stamp = datetime.now().strftime("%Y-%m-%d %H:%M")
    subject = f"Add {len(written)} call transcript{'s' if len(written) != 1 else ''} ({stamp})"
    git(out, "commit", "-q", "-m", subject, "-m", "\n".join(f"- {p}" for p in written), "--", *written)
    if not has_remote:
        print(f"committed (no remote configured): {subject}")
        return
    for attempt in (1, 2):
        try:
            git(out, "push", "-q", "origin", "HEAD")
            print(f"committed and pushed: {subject}")
            return
        except subprocess.CalledProcessError as e:
            if attempt == 2:
                print(f"committed but push failed (will retry next run): {e.stderr.strip()}", file=sys.stderr)
                return
            git(out, "pull", "-q", "--no-rebase", "--no-edit", "origin", "HEAD")


def local_timezone():
    tz = datetime.now().astimezone().tzinfo
    name = os.environ.get("TZ") or ""
    if not name:
        try:
            link = os.readlink("/etc/localtime")
            name = link.split("zoneinfo/", 1)[1] if "zoneinfo/" in link else ""
        except OSError:
            name = ""
    return tz, name


def run(args):
    os.umask(0o077)
    args.out.mkdir(parents=True, exist_ok=True, mode=0o700)
    args.state.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    summarizer = claude_summarizer(args.model) if args.summarize else None
    with (args.state.parent / ".apple-call-recordings.lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            print("another call export is running; skipped")
            return
        tz, tz_name = local_timezone()
        counts, written = export_calls(
            args.db, args.out, args.state, tz=tz, tz_name=tz_name, summarizer=summarizer,
            settle_s=max(0, args.settle_minutes) * 60,
        )
        print("calls: " + ", ".join(f"{counts[k]} {k}" for k in ("new", "updated", "unchanged", "pending", "skipped")))
        if args.commit:
            commit(args.out, written)


def main(argv=None):
    args = parser().parse_args(argv)
    try:
        run(args)
    except sqlite3.Error:
        print("Cannot read the Notes database. Check --db and macOS Full Disk Access; see docs/APPLE-NOTES.md.", file=sys.stderr)
        return 1
    except (OSError, RuntimeError, ValueError, subprocess.SubprocessError) as error:
        print(f"call export failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
