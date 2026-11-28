#!/usr/bin/env python3
"""Install/uninstall the optional macOS Apple Notes export schedules (launchd).

    schedule.py -- --out ~/AppleNotesSync --commit                    # notes (sync.py)
    schedule.py --job calls -- --out ~/Transcripts --summarize --commit   # call recordings (calls.py)
"""
import argparse
import os
import plistlib
import subprocess
import sys
from pathlib import Path

from calls import parser as calls_parser
from sync import parser as sync_parser

LABEL = "org.augmentagent.apple-notes-sync"
# job -> (launchd label, script, its argument parser, path options to pin)
JOBS = {
    "notes": (LABEL, "sync.py", sync_parser, ("db", "out")),
    "calls": ("org.augmentagent.apple-call-recordings", "calls.py", calls_parser, ("db", "out", "state")),
}


def strip_options(argv, options):
    """Drop each `--opt value` / `--opt=value` in `options` from argv by
    position. Matching on the value instead leaves the original token behind
    whenever it differs from the resolved path (relative, `~`, symlinks)."""
    out = []
    skip = False
    for token in argv:
        if skip:
            skip = False
        elif token in options:
            skip = True
        elif not any(token.startswith(opt + "=") for opt in options):
            out.append(token)
    return out


def make_plist(sync_args, interval, home=None, job="notes"):
    home = Path(home) if home is not None else Path.home()
    label, script, _, _ = JOBS[job]
    log = home / f"Library/Logs/augmentagent/{label.rsplit('.', 1)[1]}.log"
    return {
        "Label": label,
        "ProgramArguments": [sys.executable, str(Path(__file__).with_name(script)), *sync_args],
        "StartInterval": interval * 60,
        "RunAtLoad": True,
        "Umask": 0o077,
        "EnvironmentVariables": {
            "HOME": str(home),
            # ~/.local/bin is where the `claude` CLI installs (calls --summarize).
            "PATH": f"{Path(sys.executable).parent}:{home / '.local/bin'}:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin",
        },
        "StandardOutPath": str(log),
        "StandardErrorPath": str(log),
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__, epilog="Pass exporter options after --. See docs/APPLE-NOTES.md.")
    ap.add_argument("--job", choices=sorted(JOBS), default="notes",
                    help="notes: export notes (sync.py); calls: export call recordings (calls.py)")
    ap.add_argument("--uninstall", action="store_true")
    ap.add_argument("--interval", type=int, default=2, metavar="MINUTES")
    ap.add_argument("sync_args", nargs=argparse.REMAINDER)
    args = ap.parse_args()
    if sys.platform != "darwin":
        ap.error("install this job on the Mac that has Notes")
    if args.interval < 1:
        ap.error("--interval must be positive")
    sync_args = args.sync_args
    if sync_args[:1] == ["--"]:
        sync_args = sync_args[1:]
    label, _, job_parser, pinned = JOBS[args.job]
    if not args.uninstall:
        parsed = job_parser().parse_args(sync_args)
        # Persist absolute paths rather than depending on launchd's working directory.
        sync_args = strip_options(sync_args, [f"--{name}" for name in pinned])
        for name in pinned:
            sync_args += [f"--{name}", str(getattr(parsed, name))]
    domain = f"gui/{os.getuid()}"
    service = f"{domain}/{label}"
    plist = Path.home() / f"Library/LaunchAgents/{label}.plist"
    loaded = subprocess.run(["launchctl", "print", service], capture_output=True).returncode == 0
    if loaded:
        subprocess.run(["launchctl", "bootout", service], check=True)
    if args.uninstall:
        plist.unlink(missing_ok=True)
        print("schedule removed; exported notes retained")
        return
    os.umask(0o077)
    plist.parent.mkdir(parents=True, exist_ok=True)
    log_dir = Path.home() / "Library/Logs/augmentagent"
    log_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    plist.write_bytes(plistlib.dumps(make_plist(sync_args, args.interval, job=args.job)))
    subprocess.run(["launchctl", "enable", service], check=True)
    subprocess.run(["launchctl", "bootstrap", domain, str(plist)], check=True)
    print(f"Installed {label}: runs now, at login, and every {args.interval} minutes while awake.")
    print(f"Logs: {log_dir / (label.rsplit('.', 1)[1] + '.log')}")
    print(f"Scheduled Python executable (for Full Disk Access, if macOS asks): {sys.executable}")


if __name__ == "__main__":
    main()
