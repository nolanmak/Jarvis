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
    schema/*.md)                                        MATCHED+="$f"$'\n' ;;
    skills/*/SKILL.md|skills/*/*.md|skills/*.md)        MATCHED+="$f"$'\n' ;;
    crates/augmentagent-channel-core/src/reasoner.rs)   MATCHED+="$f"$'\n' ;;
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
This PR's diff vs ${DIFF_BASE} touches agent-facing prompt / skill / reasoner-config files:

$(printf '%s' "$MATCHED" | sed 's/^/  - /')

Those changes can pass cargo test + compile cleanly while still being silently rejected at runtime by the claude permission matcher (see #214). The project's gate requires a local end-to-end verification before opening a PR.

How to satisfy the gate:

  1. Pick the right verification path for what you changed:
     - schema/wiki-ask.md or any \`ask_opts\` allowlist / prompt change:
         ./target/release/augmentagent --wiki-dir ./wiki wiki ask "<question that exercises the change>"
     - schema/wiki-skill.md or wiki-ingest path change:
         ./target/release/augmentagent --wiki-dir ./wiki --wiki-schema ./schema/wiki-skill.md poll-once
     - other reasoner _opts function or schema/*.md:
         drive the matching CLI subcommand and confirm output.
     - skills/<name>/SKILL.md:
         invoke the skill in a fresh claude session and confirm behaviour.

  2. Write the receipt:
       mkdir -p .claude/agent-test-receipts
       echo "command: <what you ran>" > .claude/agent-test-receipts/${HEAD_SHA}.txt
       echo "observed: <one-line summary of the output>" >> .claude/agent-test-receipts/${HEAD_SHA}.txt
       echo "verifies: <which changed file the test exercises>" >> .claude/agent-test-receipts/${HEAD_SHA}.txt

  3. Re-run \`gh pr create\`. The receipt path is keyed by HEAD sha, so a
     rebase or amend voids the receipt — re-verify after any rewrite.

The receipt is on-disk paper trail for human review; the hook does not
parse its contents. The gate exists because #209/#211/#213 (and the
follow-on #215 that finally fixed the root cause) all shipped untested
and broke the agent for ~24h. Don't repeat that.
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
