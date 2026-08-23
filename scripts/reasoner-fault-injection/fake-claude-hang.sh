#!/usr/bin/env bash
# Fault-injection stub for the provider fallback chain (#655/#666).
# See docs/REASONER-FAULT-INJECTION.md for the rig and its env knobs.
#
# A `claude` CLI that accepts the prompt and then never answers — the shape
# the #656 watchdog exists for. Pair it with a short
# AUGMENTAGENT_REASONER_TIMEOUT_SECS.
#
# `exec` matters: bash does not forward the SIGKILL that `kill_on_drop`
# sends, so without it the sleeper would outlive the killed wrapper and
# hold the adapter's stdout pipe open.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"

fake_drain_stdin
fake_count claude

exec sleep 600
