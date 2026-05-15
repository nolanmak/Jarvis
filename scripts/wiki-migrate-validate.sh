#!/usr/bin/env bash
#
# wiki-migrate-validate.sh — count v2 adoption across wiki/people/.
#
# Run before and after `augmentagent wiki migrate --to v2` to track
# coverage. Implements the validation checks from #78 §4.
#
# Usage:
#   scripts/wiki-migrate-validate.sh [wiki_root]
#
# Defaults wiki_root to ./wiki. Reads-only; never mutates the wiki.

set -uo pipefail

WIKI_ROOT="${1:-./wiki}"
PEOPLE_DIR="$WIKI_ROOT/people"

if [[ ! -d "$PEOPLE_DIR" ]]; then
  echo "error: $PEOPLE_DIR is not a directory" >&2
  exit 1
fi

# `grep -l` exits 1 when no matches — that's a normal "zero pages" outcome
# here, not an error. We deliberately omit `set -e` to avoid aborting the
# whole script on those zeros, and rely on `wc -l` to materialise the count.
count_matching() {
  local pattern="$1"
  grep -lE "$pattern" "$PEOPLE_DIR"/*.md 2>/dev/null | wc -l
}

# Total person pages.
TOTAL=$(find "$PEOPLE_DIR" -maxdepth 1 -type f -name '*.md' | wc -l)

# Pages with any v2 frontmatter field at column 0 (top-level YAML key).
# `affiliations:` / `events:` / `introduced_by:` are ingest-written;
# `topics:` / `cadence:` / `trust:` are user-set; `strength:` is derived.
V2_POPULATED=$(count_matching '^(affiliations|events|introduced_by|topics|cadence|trust|strength):')

# Pages that explicitly carry the migration marker (skip-on-rerun signal).
MIGRATED=$(count_matching '^migrated:')

# Pages that didn't begin with `---\n` (likely no frontmatter at all).
NO_FM=0
while IFS= read -r -d '' file; do
  IFS= read -r first_line <"$file" || true
  if [[ "$first_line" != "---" ]]; then
    NO_FM=$((NO_FM + 1))
  fi
done < <(find "$PEOPLE_DIR" -maxdepth 1 -type f -name '*.md' -print0)

# Per-field breakdown for spot-checking the migration's coverage.
AFFILIATIONS=$(count_matching '^affiliations:')
EVENTS=$(count_matching '^events:')
INTRO_BY=$(count_matching '^introduced_by:')
TOPICS=$(count_matching '^topics:')
CADENCE=$(count_matching '^cadence:')
TRUST=$(count_matching '^trust:')
STRENGTH=$(count_matching '^strength:')

echo "wiki v2 adoption — $WIKI_ROOT"
echo "  total person pages     : $TOTAL"
echo "  v2-populated (any v2)  : $V2_POPULATED"
echo "  migrated marker        : $MIGRATED"
echo "  no frontmatter         : $NO_FM"
echo "  -- per field --"
echo "  affiliations           : $AFFILIATIONS"
echo "  events                 : $EVENTS"
echo "  introduced_by          : $INTRO_BY"
echo "  topics    (user)       : $TOPICS"
echo "  cadence   (user)       : $CADENCE"
echo "  trust     (user)       : $TRUST"
echo "  strength  (derived)    : $STRENGTH"
