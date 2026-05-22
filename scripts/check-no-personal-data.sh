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

set -uo pipefail
cd "$(git rev-parse --show-toplevel 2>/dev/null || echo .)"

mode="${1:-staged}"
if [ "$mode" = "--tracked" ]; then
  mapfile -t files < <(git ls-files)
  scan_added_only=0
elif [ "$mode" = "staged" ]; then
  mapfile -t files < <(git diff --cached --name-only --diff-filter=ACM)
  # pre-commit mode: scan added lines only (not whole files); avoids tripping on
  # pre-existing strings the current commit doesn't touch.
  scan_added_only=1
else
  files=("$@")
  scan_added_only=0
fi
[ "${#files[@]}" -eq 0 ] && exit 0

# Filenames that must never be tracked at all.
BANNED_NAMES='(^|/)\.env$|(^|/)\.env\.[^e]|\.db$|\.db-(wal|shm)$|discord-creds.*\.json$|tenant\.env$|\.pem$|id_rsa|\.p12$'

# Content patterns: secret shapes + PII. example.com / localhost are allowed.
read -r -d '' PATTERNS <<'PAT' || true
-----BEGIN [A-Z ]*PRIVATE KEY-----
ghp_[A-Za-z0-9]{30,}
xox[baprs]-[A-Za-z0-9-]{10,}
sk-[A-Za-z0-9]{20,}
AIza[A-Za-z0-9_-]{30,}
\b[0-9]{3}[-.][0-9]{3}[-.][0-9]{4}\b
[A-Za-z0-9._%+-]+@(?!example\.com|localhost)[A-Za-z0-9.-]+\.[A-Za-z]{2,}
(secret|api[_-]?key|token|password)\s*[=:]\s*["'][^"']{12,}["']
PAT

fail=0
for f in "${files[@]}"; do
  if printf '%s\n' "$f" | grep -qE "$BANNED_NAMES"; then
    echo "BLOCKED (must be gitignored, never tracked): $f" >&2
    fail=1
    continue
  fi
  case "$f" in
    *.example|*.example.*|.env.example|*/SECURITY.md|*/check-no-personal-data.sh) continue ;;
  esac
  if [ "$scan_added_only" -eq 1 ]; then
    # Only the lines this commit is adding — strip the leading '+' and the
    # '+++ b/path' header so patterns see real content.
    added=$(git diff --cached -U0 --diff-filter=AM -- "$f" \
      | grep -E '^\+' | grep -v '^+++ ' | sed 's/^+//')
    [ -z "$added" ] && continue
    while IFS= read -r pat; do
      [ -z "$pat" ] && continue
      if hits=$(printf '%s\n' "$added" | grep -nP "$pat" 2>/dev/null); then
        echo "POSSIBLE secret/PII in $f (added lines):" >&2
        echo "$hits" | sed 's/^/  /' >&2
        fail=1
      fi
    done <<< "$PATTERNS"
  else
    [ -f "$f" ] || continue
    while IFS= read -r pat; do
      [ -z "$pat" ] && continue
      if hits=$(grep -nPI "$pat" "$f" 2>/dev/null); then
        echo "POSSIBLE secret/PII in $f:" >&2
        echo "$hits" | sed 's/^/  /' >&2
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
