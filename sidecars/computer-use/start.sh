#!/usr/bin/env bash
set -euo pipefail
script_dir="."
if [[ "$0" == */* ]]; then script_dir="${0%/*}"; fi
here="$(cd "$script_dir" && pwd)"
python="${JARVIS_COMPUTER_PYTHON:-$(command -v python3 || true)}"
node="${JARVIS_COMPUTER_NODE:-$(command -v node || true)}"
if [ -z "$python" ] || [ -z "$node" ]; then
  printf 'computer-use worker requires Python 3 and Node.js on PATH\n' >&2
  exit 1
fi
exec "$python" "$here/lease.py" "$node" "$here/server.mjs"
