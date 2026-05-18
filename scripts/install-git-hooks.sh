#!/usr/bin/env bash
# Install the personal-data/secret pre-commit hook. Idempotent.
#   ./scripts/install-git-hooks.sh
set -euo pipefail
ROOT="$(git rev-parse --show-toplevel)"
HOOK="$ROOT/.git/hooks/pre-commit"
cat > "$HOOK" <<'EOF'
#!/usr/bin/env bash
exec "$(git rev-parse --show-toplevel)/scripts/check-no-personal-data.sh" staged
EOF
chmod +x "$HOOK"
echo "Installed pre-commit hook → scripts/check-no-personal-data.sh"
echo "Bypass once (rarely): git commit --no-verify"
