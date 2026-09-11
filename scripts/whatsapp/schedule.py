#!/usr/bin/env python3
"""Install/uninstall the optional macOS WhatsApp export schedule."""
import argparse
import os
from pathlib import Path
import plistlib
import subprocess
import sys

from sync import parser as sync_parser

LABEL = "org.augmentagent.whatsapp-history-sync"


def make_plist(sync_args, interval, home=None):
    home = Path(home) if home is not None else Path.home()
    log = home / "Library/Logs/augmentagent/whatsapp-history-sync.log"
    return {
        "Label": LABEL,
        "ProgramArguments": [sys.executable, str(Path(__file__).with_name("sync.py")), *sync_args],
        "StartInterval": interval * 60,
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
    ap = argparse.ArgumentParser(description=__doc__, epilog="Pass exporter options after --. See docs/WHATSAPP-HISTORY.md.")
    ap.add_argument("--uninstall", action="store_true")
    ap.add_argument("--interval", type=int, default=30, metavar="MINUTES")
    ap.add_argument("sync_args", nargs=argparse.REMAINDER)
    args = ap.parse_args()
    if sys.platform != "darwin":
        ap.error("install this job on the Mac that has WhatsApp Desktop; the receiving agent can run on Linux")
    if args.interval < 1:
        ap.error("--interval must be positive")
    sync_args = args.sync_args
    if sync_args[:1] == ["--"]:
        sync_args = sync_args[1:]
    if not args.uninstall:
        parsed = sync_parser().parse_args(sync_args)
        # Persist absolute paths rather than depending on launchd's working directory.
        sync_args += ["--db", str(parsed.db), "--out", str(parsed.out)]
    domain = f"gui/{os.getuid()}"
    service = f"{domain}/{LABEL}"
    plist = Path.home() / f"Library/LaunchAgents/{LABEL}.plist"
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
    plist.write_bytes(plistlib.dumps(make_plist(sync_args, args.interval)))
    subprocess.run(["launchctl", "enable", service], check=True)
    subprocess.run(["launchctl", "bootstrap", domain, str(plist)], check=True)
    print(f"Installed {LABEL}: runs now, at login, and every {args.interval} minutes while awake.")
    print(f"Logs: {log_dir / 'whatsapp-history-sync.log'}")
    print(f"Scheduled Python executable (for Full Disk Access): {sys.executable}")


if __name__ == "__main__":
    main()
