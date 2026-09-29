#!/usr/bin/env python3
"""Install/uninstall the optional macOS Messages export or send schedule."""
import argparse
import os
from pathlib import Path
import plistlib
import subprocess
import sys

from sync import parser as sync_parser

LABEL = "org.augmentagent.imessage-sync"
# job → (launchd label, script, log name)
JOBS = {
    "sync": (LABEL, "sync.py", "imessage-sync.log"),
    "send": ("org.augmentagent.imessage-send", "send.py", "imessage-send.log"),
}


def make_plist(sync_args, interval, home=None, job="sync", interval_s=None):
    home = Path(home) if home is not None else Path.home()
    label, script, log_name = JOBS[job]
    log = home / "Library/Logs/augmentagent" / log_name
    return {
        "Label": label,
        "ProgramArguments": [sys.executable, str(Path(__file__).with_name(script)), *sync_args],
        "StartInterval": interval_s if interval_s is not None else interval * 60,
        "RunAtLoad": True,
        "Umask": 0o077,
        "EnvironmentVariables": {
            "HOME": str(home),
            "PATH": f"{Path(sys.executable).parent}:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin",
        },
        "StandardOutPath": str(log),
        "StandardErrorPath": str(log),
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__, epilog="Pass exporter options after --. See docs/IMESSAGE.md.")
    ap.add_argument("--uninstall", action="store_true")
    ap.add_argument("--job", choices=sorted(JOBS), default="sync",
                    help="sync exports history; send delivers approved replies (#1305)")
    ap.add_argument("--interval", type=int, default=30, metavar="MINUTES")
    ap.add_argument("--every-seconds", type=int, default=15,
                    help="send job only: how often to check the agent outbox")
    ap.add_argument("sync_args", nargs=argparse.REMAINDER)
    args = ap.parse_args()
    if sys.platform != "darwin":
        ap.error("install this job on the Mac that has Messages; the receiving agent can run on Linux")
    if args.interval < 1 or args.every_seconds < 5:
        ap.error("--interval must be positive and --every-seconds at least 5")
    label, _, log_name = JOBS[args.job]
    sync_args = args.sync_args
    if sync_args[:1] == ["--"]:
        sync_args = sync_args[1:]
    if not args.uninstall and args.job == "sync":
        parsed = sync_parser().parse_args(sync_args)
        # Persist absolute paths rather than depending on launchd's working directory.
        sync_args += ["--db", str(parsed.db), "--out", str(parsed.out)]
    if not args.uninstall and args.job == "send":
        from send import parser as send_parser
        parsed = send_parser().parse_args(sync_args)
        sync_args += ["--db", str(parsed.db), "--state-dir", str(parsed.state_dir)]
    domain = f"gui/{os.getuid()}"
    service = f"{domain}/{label}"
    plist = Path.home() / f"Library/LaunchAgents/{label}.plist"
    loaded = subprocess.run(["launchctl", "print", service], capture_output=True).returncode == 0
    if loaded:
        subprocess.run(["launchctl", "bootout", service], check=True)
    if args.uninstall:
        plist.unlink(missing_ok=True)
        print("schedule removed; exported messages retained")
        return
    os.umask(0o077)
    plist.parent.mkdir(parents=True, exist_ok=True)
    log_dir = Path.home() / "Library/Logs/augmentagent"
    log_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    interval_s = args.every_seconds if args.job == "send" else None
    plist.write_bytes(plistlib.dumps(
        make_plist(sync_args, args.interval, job=args.job, interval_s=interval_s)))
    subprocess.run(["launchctl", "enable", service], check=True)
    subprocess.run(["launchctl", "bootstrap", domain, str(plist)], check=True)
    every = f"{interval_s} seconds" if interval_s else f"{args.interval} minutes"
    print(f"Installed {label}: runs now, at login, and every {every} while awake.")
    print(f"Logs: {log_dir / log_name}")
    print(f"Scheduled Python executable (for Full Disk Access): {sys.executable}")


if __name__ == "__main__":
    main()
