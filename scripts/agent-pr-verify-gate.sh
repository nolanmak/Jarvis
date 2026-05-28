#!/usr/bin/env bash
# PreToolUse hook: blocks `gh pr create` when the branch's diff vs main
# touches any agent prompt / skill / reasoner-config file, unless the
# author has dropped a verification receipt at
# `.claude/agent-test-receipts/<HEAD-sha>.txt` proving they tested the
# change locally.
#
# Why this exists: PRs #209/#211/#213 shipped wiki-agent prompt + allowlist
# changes that compiled clean and passed unit tests but were silently
# rejected at runtime by the harness because nobody exercised the actual
# code path (`augmentagent wiki ask`). Each PR claimed to fix the issue,
# none did, the user lost ~24h to it. This hook forces a real local
# invocation of the affected agent before merging.
#
# Reads one tool-call JSON event on stdin (Claude Code PreToolUse hook
# protocol) and either exits 0 (allow) or prints a block-decision JSON
# and exits 0 (claude reads the JSON, surfaces `reason` to the model so it
# can adjust and retry). Exit 2 = hook itself broke (treated as block).
#
# Trigger surface: matched on the Bash tool only; the hook itself short-
# circuits to allow for any command that is not `gh pr create …`.

set -euo pipefail

# Defensive: jq is on every box this hook runs on (the existing
# `aa-wiki-scope-guard.sh` already requires it). Fail loud if missing.
if ! command -v jq >/dev/null 2>&1; then
  echo "agent-pr-verify-gate: jq missing on PATH" >&2
  exit 2
fi

INPUT=$(cat)

TOOL=$(printf '%s' "$INPUT" | jq -r '.tool_name // empty')
if [[ "$TOOL" != "Bash" ]]; then
  exit 0
fi

CMD=$(printf '%s' "$INPUT" | jq -r '.tool_input.command // empty')
if [[ -z "$CMD" ]]; then
  exit 0
fi

# Recognise `gh pr create` regardless of leading env vars, flags, or
# trailing pipes. We match the substring `gh pr create` with optional
# leading whitespace; this catches all forms (`gh pr create`,
# `GH_TOKEN=… gh pr create`, etc.) without trying to fully parse the
# shell command, which is brittle.
if ! grep -Eq '(^|[^a-zA-Z0-9_-])gh[[:space:]]+pr[[:space:]]+create([[:space:]]|$)' <<<"$CMD"; then
  exit 0
fi

# We need a repo to diff against. If not in a git tree, allow — the
# user's gh invocation will fail on its own with a clearer message than
# we could produce.
if ! REPO_ROOT=$(git rev-parse --show-toplevel 2>/dev/null); then
  exit 0
fi

# Best-effort: refresh the local main ref so the diff is honest. Failures
# (offline, no remote, etc.) are ignored — we fall back to whatever main
# we have locally rather than blocking the PR over a network blip.
git -C "$REPO_ROOT" fetch --quiet origin main 2>/dev/null || true

# Determine the merge-base diff target. `origin/main...HEAD` (three dots)
# = "everything on HEAD not yet on origin/main", which is the PR's
# actual contribution. If origin/main isn't resolvable for any reason
# (fresh clone, missing remote), fall back to local `main`; if THAT
# fails, allow — better to skip the gate than to misfire on a misconfigured
# repo.
DIFF_BASE=""
if git -C "$REPO_ROOT" rev-parse --verify --quiet origin/main >/dev/null; then
  DIFF_BASE="origin/main"
elif git -C "$REPO_ROOT" rev-parse --verify --quiet main >/dev/null; then
  DIFF_BASE="main"
else
  exit 0
fi

CHANGED=$(git -C "$REPO_ROOT" diff --name-only "${DIFF_BASE}...HEAD" 2>/dev/null || true)
if [[ -z "$CHANGED" ]]; then
  # No diff vs main — gh will reject the PR creation itself; let it.
  exit 0
fi

# Files that count as "agent prompt or skill" per the scope decision.
# Three buckets:
#   1. `schema/*.md`         — every agent system prompt loaded via include_str!
#   2. `skills/**/*.md`      — every Claude Code skill prompt
#   3. reasoner-config Rust  — `reasoner.rs` wires prompts to allowlists +
#                              env vars, and silent bugs there (like #214)
#                              are exactly what this gate exists to catch.
#
# Patterns are evaluated as fixed POSIX globs against each changed path.
# A single match means "verification required".
MATCHED=""
while IFS= read -r f; do
  [[ -z "$f" ]] && continue
  case "$f" in
    # Original (#216) match set: agent prompts, skills, reasoner config.
    schema/*.md)                                        MATCHED+="$f"$'\n' ;;
    skills/*/SKILL.md|skills/*/*.md|skills/*.md)        MATCHED+="$f"$'\n' ;;
    crates/augmentagent-channel-core/src/reasoner.rs)   MATCHED+="$f"$'\n' ;;
    # #229 widening: any channel triage / poller / observer logic. Every
    # PR in the 217/218/219/220/222 batch unit-tested clean but went
    # unverified against live email — the user was burned twice trusting
    # "cargo test passes" as proof of working triage. Files in this set
    # change runtime behaviour the unit tests can't fully model (LLM
    # gating order, Gmail API responses, sweep timing) and demand a real
    # exercise against the running daemon or a `poll-once` invocation
    # before merge.
    crates/augmentagent-channel-email/src/channel.rs)      MATCHED+="$f"$'\n' ;;
    crates/augmentagent-channel-email/src/outbound.rs)     MATCHED+="$f"$'\n' ;;
    crates/augmentagent-channel-email/src/trigger.rs)      MATCHED+="$f"$'\n' ;;
    crates/augmentagent-channel-core/src/trigger.rs)       MATCHED+="$f"$'\n' ;;
    crates/augmentagent-channel-core/src/governor/*.rs)    MATCHED+="$f"$'\n' ;;
    # Any new triage / observer file under a channel crate — catches
    # future modules like sigextract.rs additions that change triage
    # classification.
    crates/augmentagent-channel-*/src/sigextract.rs)       MATCHED+="$f"$'\n' ;;
    crates/augmentagent-channel-*/src/tone.rs)             MATCHED+="$f"$'\n' ;;
    # The Discord broker's message handler — same class of runtime
    # behaviour gap (e.g. !invoice / !loops / `/loop` dispatch). Past
    # PRs here have shipped clean-compiling but actually-broken
    # invocations because cargo test doesn't exercise serenity events.
    crates/augmentagent-approval-discord/src/event_handler.rs)   MATCHED+="$f"$'\n' ;;
    crates/augmentagent-approval-discord/src/loops.rs)           MATCHED+="$f"$'\n' ;;
    crates/augmentagent-approval-discord/src/process_loops.rs)   MATCHED+="$f"$'\n' ;;
  esac
done <<<"$CHANGED"

if [[ -z "$MATCHED" ]]; then
  exit 0
fi

# Verification receipt path is keyed by HEAD sha so a rebase / amend
# forces a fresh test rather than letting a stale receipt unlock the
# gate.
HEAD_SHA=$(git -C "$REPO_ROOT" rev-parse HEAD)
RECEIPT="$REPO_ROOT/.claude/agent-test-receipts/${HEAD_SHA}.txt"

if [[ -f "$RECEIPT" && -s "$RECEIPT" ]]; then
  exit 0
fi

# Block. Tell the agent EXACTLY what to do — the failure mode here is
# always "agent doesn't know the project has a wiki ask verification
# path", so spell out the command, the receipt path, and a one-line
# explanation of why.
REASON=$(cat <<EOF
This PR's diff vs ${DIFF_BASE} touches runtime-behaviour files that cargo test cannot fully model:

$(printf '%s' "$MATCHED" | sed 's/^/  - /')

The gate exists because PRs in this codebase have repeatedly compiled clean + passed unit tests while being silently broken at runtime (see #214, and the entire #217/#218/#219/#220/#222 batch where 5 of 6 PRs shipped without ever being exercised against live email / running daemon). "cargo test passes" is not proof the change works. The project requires a real end-to-end exercise before opening a PR.

How to satisfy the gate — pick the verification path matching your changed file:

  schema/wiki-ask.md OR crates/augmentagent-channel-core/src/reasoner.rs (ask_opts/allowlist/prompt):
      ./target/release/augmentagent --wiki-dir ./wiki wiki ask "<question that exercises the change>"
      → observe: the agent actually invokes the new tool / sees the new prompt section.

  schema/wiki-skill.md OR wiki-ingest path:
      ./target/release/augmentagent --wiki-dir ./wiki --wiki-schema ./schema/wiki-skill.md poll-once
      → observe: ingest writes the expected wiki/people or wiki/log entry.

  crates/augmentagent-channel-email/src/channel.rs (triage gate logic):
      Seed a synthetic email in the emails table OR set AUGMENTAGENT_DRY_RUN=true and trigger poll-once,
      then watch ~/.local/state/augmentagent/stdout.log for your new log prefix
      ([skip:automated], [ingest-only:event-blast], [skip:already-replied], etc.).
      → observe: the log line your gate emits actually appears for a matching input.

  crates/augmentagent-channel-email/src/outbound.rs OR sigextract/tone (signal extraction):
      cargo run --release -p augmentagent-cli -- <relevant subcommand> on a known input,
      OR add an integration test that runs against a fixture file and assert the observable signal.

  crates/augmentagent-channel-*/src/trigger.rs OR core/governor (poller / rate logic):
      Run the daemon briefly (./target/release/augmentagent serve --dry-run true) and confirm the new
      behaviour appears in stdout/stderr logs within one poll cycle.

  crates/augmentagent-approval-discord/src/event_handler.rs OR loops.rs OR process_loops.rs:
      Test against a live Discord DM (cheapest) OR run the local repro with serenity's test harness
      if one exists. cargo test alone has historically missed serenity-event bugs.

  skills/<name>/SKILL.md:
      Invoke the skill in a fresh claude session and confirm the behaviour.

Then write the receipt (this satisfies the gate — content is for human review, not parsed):

  mkdir -p .claude/agent-test-receipts
  cat > .claude/agent-test-receipts/${HEAD_SHA}.txt <<RECEIPT
  command: <what you ran>
  observed: <one-line summary; quote the relevant log line / output snippet>
  verifies: <which changed file the test exercises>
  RECEIPT

Re-run \`gh pr create\`. The receipt path is keyed by HEAD sha, so any rebase or amend voids the receipt — re-verify after any rewrite.

If you genuinely cannot live-exercise a change (e.g. a refactor with no observable behaviour delta), say so in the receipt — write "command: n/a refactor", "observed: cargo test green + no behaviour delta possible", "verifies: <files>". The hook does not parse the receipt; it only checks that the file exists and is non-empty. But document HONESTLY — a future agent reading the audit trail will catch lies.
EOF
)

jq -n --arg r "$REASON" '
  {
    decision: "block",
    reason: $r,
    hookSpecificOutput: {
      hookEventName: "PreToolUse",
      permissionDecision: "deny",
      permissionDecisionReason: $r
    }
  }
'
exit 0
