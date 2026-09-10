#!/usr/bin/env bash
# check-no-personal-data.sh — block secrets / PII from entering git.
#
#   ./scripts/check-no-personal-data.sh            # scan STAGED changes (hook use)
#   ./scripts/check-no-personal-data.sh --tracked  # scan all tracked files (audit)
#   ./scripts/check-no-personal-data.sh f1 f2 ...   # scan specific files
#
# Exit 1 (with the offending file:line) if anything matches. Install as a
# pre-commit hook via ./scripts/install-git-hooks.sh. Not exhaustive — a
# backstop, not a substitute for not hardcoding personal data.
#
# Allow-marker: a line containing `pii-ok` (e.g. `// pii-ok` in Rust/TS,
# `# pii-ok` in shell/Python) exempts reviewed fixtures except for common
# personal mailbox providers, which are always blocked. Intended ONLY for test-fixture
# data where the pattern is real but the value is synthetic, e.g.
# `"newsletter@brand.example.com"` in a unit test that verifies a matcher.

set -uo pipefail
cd "$(git rev-parse --show-toplevel 2>/dev/null || echo .)"

mode="${1:-staged}"
if [ "$mode" = "--tracked" ]; then
  mapfile -t files < <(git ls-files)
  scan_staged=0
elif [ "$mode" = "staged" ]; then
  mapfile -t files < <(git diff --cached --name-only --diff-filter=ACMR)
  # Scan the staged blob, including renames, rather than an unstaged cleanup.
  scan_staged=1
else
  files=("$@")
  scan_staged=0
fi
[ "${#files[@]}" -eq 0 ] && exit 0

# Filenames that must never be tracked at all.
BANNED_NAMES='(^|/)\.env$|(^|/)\.env\.(?!example$)|\.db$|\.db-(wal|shm)$|discord-creds.*\.json$|tenant\.env$|\.pem$|id_rsa|\.p12$|(^|/)(linkedin|twitter|instagram)-(auth|cookies|session)\.json$|(^|/)chrome-profile/|(^|/)[^/]*-chrome-profile/'

# Content patterns: secret shapes + PII. RFC 2606 reserved names
# (example.com/org/net and the .example/.test/.invalid TLDs) and protocol
# identifiers are allowed; these are not personal mailboxes.
read -r -d '' PATTERNS <<'PAT' || true
-----BEGIN [A-Z ]*PRIVATE KEY-----
ghp_[A-Za-z0-9]{30,}
github_pat_[A-Za-z0-9_]{30,}
xox[baprs]-[A-Za-z0-9-]{10,}
sk-(?:proj-|ant-)?[A-Za-z0-9_-]{20,}
AIza[A-Za-z0-9_-]{30,}
\b[0-9]{3}[-.](?!555[-.])[0-9]{3}[-.][0-9]{4}\b
(?i)(?<![A-Za-z0-9._%+-])[A-Za-z0-9._%+-]+@(?!(?:[A-Za-z0-9-]+\.)*example\.(?:com|org|net)(?![A-Za-z0-9.-])|(?:[A-Za-z0-9-]+\.)*(?:example|test|invalid)(?![A-Za-z0-9.-])|localhost(?![A-Za-z0-9.-])|s\.whatsapp\.net(?![A-Za-z0-9.-])|g\.us(?![A-Za-z0-9.-])|users\.noreply\.github\.com(?![A-Za-z0-9.-]))[A-Za-z0-9.-]+\.[A-Za-z]{2,}
(secret|api[_-]?key|token|password)\s*[=:]\s*["'][^"']{12,}["']
PAT

# Personal mailbox providers are never exempted by an inline marker. Their
# fixtures can use reserved example domains without changing parser behavior.
PERSONAL_MAIL_PATTERN='(?i)(?<![A-Za-z0-9._%+-])[A-Za-z0-9._%+-]+@(?:gmail|googlemail|outlook|hotmail|live|yahoo|icloud|protonmail|proton)\.com\b|(?i)(?<![A-Za-z0-9._%+-])[A-Za-z0-9._%+-]+@proton\.me\b'
PATTERNS+=$'\n'"$PERSONAL_MAIL_PATTERN"

filter_fixture_exemptions() {
  if [[ "$1" == "$PERSONAL_MAIL_PATTERN" ]]; then
    cat
  else
    grep -v 'pii-ok'
  fi
}

fail=0
for f in "${files[@]}"; do
  if printf '%s\n' "$f" | grep -qP "$BANNED_NAMES"; then
    echo "BLOCKED (must be gitignored, never tracked): $f" >&2
    fail=1
    continue
  fi
  if [ "$scan_staged" -eq 1 ]; then
    if ! contents=$(git show ":$f"); then
      echo "BLOCKED (cannot read staged file): $f" >&2
      fail=1
      continue
    fi
    [ -z "$contents" ] && continue
    while IFS= read -r pat; do
      [ -z "$pat" ] && continue
      if hits=$(printf '%s\n' "$contents" | grep -nP -- "$pat" 2>/dev/null | filter_fixture_exemptions "$pat"); then
        echo "POSSIBLE secret/PII in $f (staged blob; values withheld):" >&2
        echo "$hits" | cut -d: -f1 | sed 's/^/  line /' >&2
        fail=1
      fi
    done <<< "$PATTERNS"
  else
    [ -f "$f" ] || continue
    while IFS= read -r pat; do
      [ -z "$pat" ] && continue
      if hits=$(grep -nPI -- "$pat" "$f" 2>/dev/null | filter_fixture_exemptions "$pat"); then
        echo "POSSIBLE secret/PII in $f:" >&2
        echo "$hits" | cut -d: -f1 | sed 's/^/  line /' >&2
        fail=1
      fi
    done <<< "$PATTERNS"
  fi
done

if [ "$fail" -ne 0 ]; then
  cat >&2 <<'MSG'

✗ Personal-data / secret check failed. Do NOT commit this.
  Real values belong in .env or the sqlite DB (both gitignored) — ship only
  placeholders/templates. See docs/SECURITY.md. To override a false positive:
  git commit --no-verify  (use sparingly, and double-check).
MSG
  exit 1
fi
echo "✓ no obvious secrets/PII in scanned files"
