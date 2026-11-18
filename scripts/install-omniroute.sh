#!/usr/bin/env bash
# Explicit opt-in installer. Never called by the updater or normal daemon startup.
set -euo pipefail
exec python3 "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib/omniroute_install.py" "$@"
