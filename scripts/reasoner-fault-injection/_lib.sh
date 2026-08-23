#!/usr/bin/env bash
# Shared helpers for the reasoner fault-injection stubs (#655/#666).
# Sourced, never executed directly.
#
# WHY STATE LIVES UNDER $HOME: the codex and gemini adapters spawn with
# `env_clear()` (the #128 posture) and re-add only OS essentials, so a
# `FAKE_*` env var set by the test would reach the claude stub and nothing
# else. HOME is the one channel every adapter forwards — point it at a
# scratch dir and the whole family writes its counters to one place.

# Where invocation counters live. Created on demand.
fake_state_dir() {
  printf '%s\n' "${HOME:-/tmp}/.fake-cli"
}

# Record one spawn of <provider>. Tests read these to assert things like
# "the latched provider was never spawned again".
fake_count() {
  local provider="$1" dir count=0
  dir="$(fake_state_dir)"
  mkdir -p "$dir"
  [[ -f "$dir/$provider.count" ]] && count=$(<"$dir/$provider.count")
  echo $((count + 1)) > "$dir/$provider.count"
}

# Consume the prompt the adapter writes to stdin. MUST run before the stub
# writes anything: an adapter's `stdin.write_all` would otherwise EPIPE
# against an exited stub and the scenario would test the wrong failure.
fake_drain_stdin() {
  cat >/dev/null
}
