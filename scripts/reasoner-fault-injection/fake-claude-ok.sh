#!/usr/bin/env bash
# Fault-injection stub for the provider fallback chain (#655/#666).
# See docs/REASONER-FAULT-INJECTION.md for the rig and its env knobs.
#
# The healthy `claude` CLI: a stream-json completion the adapter parses into
# a normal answer. The negative control — with this in place no fallback
# provider may be spawned at all.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"

fake_drain_stdin
fake_count claude

printf '{"type":"assistant","message":{"content":[{"type":"text","text":"PONG-FROM-FAKE-CLAUDE"}]}}\n'
printf '{"type":"result","result":"PONG-FROM-FAKE-CLAUDE"}\n'
