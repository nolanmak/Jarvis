#!/usr/bin/env bash
set -euo pipefail
repo_root="$(cd "$(dirname "$0")/.." && pwd)"
source "$repo_root/scripts/lib/scheduled-sync.sh"
scheduled_sync uninstall wiki "$repo_root"
