#!/bin/bash
# swarm-gate.sh — pre-flight gate for the swarm dispatcher.
#
# Default behavior (no args): run the full build/test suite that's required
# before opening a swarm-batch PR (cargo build --release, cargo test, npm
# run build). This preserves the original stub semantics.
#
# Resource checks (additive): callers that want to gate worker spawning on
# host capacity can pass:
#   --check-disk <GB>     fail if free space on / is below threshold
#                         (default 15 GB)
#   --max-worktrees <N>   fail if .claude/worktrees/ already has >= N
#                         active worktree directories (default 4)
#   --no-build            skip the cargo + npm build/test steps; useful when
#                         you only want the pre-flight resource checks
#   -h, --help            print usage and exit
#
# All resource checks always run when flags are present; --no-build only
# controls the build half. Exit code is non-zero on the first check failure
# so the dispatcher can bail before spawning.

set -euo pipefail

CHECK_DISK_GB=15
MAX_WORKTREES=4
RUN_BUILD=1

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORKTREE_DIR="$REPO_ROOT/.claude/worktrees"

usage() {
  cat <<'USAGE'
Usage: swarm-gate.sh [--check-disk <GB>] [--max-worktrees <N>] [--no-build] [-h|--help]

Pre-flight gate for the swarm dispatcher. With no arguments, runs the full
build/test suite (cargo build --release, cargo test, npm run build).

Options:
  --check-disk <GB>     Require at least <GB> free on / (default: 15).
  --max-worktrees <N>   Fail if .claude/worktrees/ has >= N active
                        worktrees (default: 4).
  --no-build            Skip cargo + npm build/test steps.
  -h, --help            Show this help and exit.

Examples:
  swarm-gate.sh                              # full build + default resource gate
  swarm-gate.sh --check-disk 20 --no-build   # pre-flight check only, 20 GB floor
  swarm-gate.sh --max-worktrees 6            # raise concurrency cap
USAGE
}

die() {
  printf '[swarm-gate ERR] %s\n' "$*" >&2
  exit 1
}

while [ $# -gt 0 ]; do
  case "$1" in
    --check-disk)
      [ $# -ge 2 ] || die "--check-disk requires a value"
      CHECK_DISK_GB="$2"
      shift 2
      ;;
    --check-disk=*)
      CHECK_DISK_GB="${1#*=}"
      shift
      ;;
    --max-worktrees)
      [ $# -ge 2 ] || die "--max-worktrees requires a value"
      MAX_WORKTREES="$2"
      shift 2
      ;;
    --max-worktrees=*)
      MAX_WORKTREES="${1#*=}"
      shift
      ;;
    --no-build)
      RUN_BUILD=0
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf '[swarm-gate ERR] unknown argument: %s\n' "$1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

# Validate numeric flag values.
case "$CHECK_DISK_GB" in
  ''|*[!0-9]*) die "--check-disk must be a non-negative integer (got: $CHECK_DISK_GB)" ;;
esac
case "$MAX_WORKTREES" in
  ''|*[!0-9]*) die "--max-worktrees must be a non-negative integer (got: $MAX_WORKTREES)" ;;
esac

# Free disk on / in GB (df reports KiB with --output=avail).
free_kib="$(df --output=avail / | tail -1 | tr -d ' ')"
free_gb=$(( free_kib / 1024 / 1024 ))

# Active worktree count. Treat a missing directory as zero.
if [ -d "$WORKTREE_DIR" ]; then
  active_worktrees=$(find "$WORKTREE_DIR" -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d ' ')
else
  active_worktrees=0
fi

printf 'Pre-flight: %s GB free, %s active worktrees\n' "$free_gb" "$active_worktrees"

if [ "$free_gb" -lt "$CHECK_DISK_GB" ]; then
  die "free space on / is ${free_gb} GB, below threshold ${CHECK_DISK_GB} GB"
fi

if [ "$active_worktrees" -ge "$MAX_WORKTREES" ]; then
  die "active worktrees=${active_worktrees} >= limit ${MAX_WORKTREES} (see scripts/worktree-prune.sh)"
fi

if [ "$RUN_BUILD" -eq 0 ]; then
  echo "Pre-flight OK (build skipped via --no-build)."
  exit 0
fi

# Original build/test gate. Sourced cargo env so the script works under
# cron/systemd where ~/.cargo/bin isn't on PATH.
if [ -f "$HOME/.cargo/env" ]; then
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
cargo build --release -p augmentagent-cli
cargo test
npm run build
