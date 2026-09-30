#!/usr/bin/env bash
# Python installers use the same validated, rollback-capable launchd transaction
# as the shell installers.
set -euo pipefail
[ "$#" -eq 4 ] || { echo 'usage: install-launchd-plist.sh LABEL PLIST CANDIDATE KICK' >&2; exit 2; }
repo="$(cd "$(dirname "$0")/../.." && pwd)"
source "$repo/scripts/lib/launchd-install.sh"
launchd_install "$1" "$2" "$3" "$4"
