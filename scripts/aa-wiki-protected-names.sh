# Shared definition sourced by the wiki PreToolUse guards
# (aa-wiki-scope-guard.sh, aa-journal-guard.sh). NOT executable on its own.
#
# #1078 — instruction/config files that Claude Code or Codex auto-load from a
# working directory (and its ancestors) on every call. A model write to any of
# these under the wiki persists as instructions into every later wiki call
# (ask, ingest, resume, lint) and survives in the private mirror, turning a
# one-off prompt injection into a durable one. The canonical copy of this list
# is PROTECTED_INJECTION_NAMES in
# crates/augmentagent-channel-core/src/codex_tools.rs; a parity test fails if
# the two drift. Both guards source this file so they cannot drift from each
# other. Callers must already have pinned `LC_ALL=C` (both do).
AA_PROTECTED_INJECTION_NAMES=("CLAUDE.md" "CLAUDE.local.md" ".claude" ".mcp.json" "AGENTS.md")

# aa_path_has_protected_injection_name <relative-path>
# Return 0 (true) if any `/`-separated component of the argument equals a
# reserved injection name, compared case-insensitively (`claude.md`, `CLAUDE.MD`
# and `Claude.local.md` all match; `people/claude-shannon.md` does not). Pass a
# path already made relative to the wiki root so the wiki root's own components
# cannot cause a false positive.
aa_path_has_protected_injection_name() {
  local path="$1" component reserved
  local IFS='/'
  for component in $path; do
    [[ -z "$component" ]] && continue
    local lower="${component,,}"
    for reserved in "${AA_PROTECTED_INJECTION_NAMES[@]}"; do
      [[ "$lower" == "${reserved,,}" ]] && return 0
    done
  done
  return 1
}
