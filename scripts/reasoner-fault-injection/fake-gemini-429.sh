#!/usr/bin/env bash
# Fault-injection stub for the provider fallback chain (#655/#666).
# See docs/REASONER-FAULT-INJECTION.md for the rig and its env knobs.
#
# Gemini out of quota: the terminal JSON error object plus a non-zero exit.
# (Stderr retry chatter alone is NOT failure — gemini-cli#17906 — so this
# stub deliberately says it on stdout, where the adapter reads it.)
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"

fake_drain_stdin
fake_count gemini

printf '%s\n' '{"error":{"type":"ApiError","message":"429 RESOURCE_EXHAUSTED: rateLimitExceeded","code":429}}'
exit 1
