#!/bin/bash
# Wrapper invoked by launchd/systemd each morning. Runs the daily research
# pipeline: pull recent arXiv AI/agent papers + latest leapmodel commits,
# compare against our agent process via the swappable LLM driver, file GitHub
# issues for the top gaps, and post a digest to Discord.
#
# Safe to run manually to preview/regenerate at any time. Add `--dry-run true`
# on the command line to preview without filing issues.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

# Best-effort vault mount (macOS sparse bundle). No-op everywhere else.
./scripts/vault-mount.sh || true

# Load .env (secrets: DISCORD_*, plus RESEARCH_* knobs). The `set -a` exports
# everything we source so the binary inherits it.
if [ -f .env ]; then
  set -a
  # shellcheck disable=SC1091
  . ./.env
  set +a
fi

exec ./target/release/augmentagent \
  --wiki-dir ./wiki \
  research --since-hours 24 --post-discord true --dry-run false --max-issues 3
