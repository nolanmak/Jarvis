#!/usr/bin/env bash
# Install the pre-commit hooks. Idempotent.
#   ./scripts/install-git-hooks.sh
#
# Three checks, cheapest first:
#   1. check-not-on-main       — never commit on the branch the updater deploys
#   2. check-no-personal-data  — no secrets/PII in the staged blob
set -euo pipefail
ROOT="$(git rev-parse --show-toplevel)"
HOOK="$ROOT/.git/hooks/pre-commit"
cat > "$HOOK" <<'EOF'
#!/usr/bin/env bash
ROOT="$(git rev-parse --show-toplevel)"
"$ROOT/scripts/check-not-on-main.sh" || exit 1
exec "$ROOT/scripts/check-no-personal-data.sh" staged
EOF
chmod +x "$HOOK"
echo "Installed pre-commit hook → check-not-on-main.sh + check-no-personal-data.sh"
echo "Bypass once (rarely): git commit --no-verify"

PRE_PUSH="$ROOT/.git/hooks/pre-push"
cat > "$PRE_PUSH" <<'EOF'
#!/usr/bin/env bash
ROOT="$(git rev-parse --show-toplevel)"
exec "$ROOT/scripts/ccat-public-push-gate.sh" "$@"
EOF
chmod +x "$PRE_PUSH"
echo "Installed pre-push hook → ccat-public-push-gate.sh (opt in with AUGMENTAGENT_CCAT_PUBLIC_REMOTES)"
