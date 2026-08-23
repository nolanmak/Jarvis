#!/usr/bin/env bash
# Fault-injection stub for the provider fallback chain (#655/#666).
# See docs/REASONER-FAULT-INJECTION.md for the rig and its env knobs.
#
# A healthy `gemini --output-format json` run: one object on stdout whose
# `.response` is the answer, exit 0.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"

fake_drain_stdin
fake_count gemini

printf '%s\n' '{"response":"PONG-FROM-FAKE-GEMINI","stats":{}}'
