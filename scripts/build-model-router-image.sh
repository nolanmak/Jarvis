#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
revision='17c4cc76877bd1755030a8414f8d0083f48dcccf'
source_dir="$(mktemp -d)"
trap 'rm -rf "$source_dir"' EXIT

git clone --quiet --filter=blob:none https://github.com/decolua/9router.git "$source_dir"
git -C "$source_dir" checkout --quiet --detach "$revision"
cp "$repo_root/sidecars/9router/package-lock.json" "$source_dir/package-lock.json"
git -C "$source_dir" apply --check "$repo_root/sidecars/9router/runpod-reconciliation.patch"
git -C "$source_dir" apply "$repo_root/sidecars/9router/runpod-reconciliation.patch"

NAMESPACED_TOOLS_MODULE="$source_dir/open-sse/translator/concerns/namespacedTools.js" \
  node --test "$repo_root/scripts/tests/model_router_namespace_test.mjs"

# Upstream's Dockerfile uses an unlocked npm install. Build this pinned image
# from the same committed dependency lock that the Linux service installer uses.
python3 - "$source_dir/Dockerfile" <<'PY'
from pathlib import Path
import sys
path = Path(sys.argv[1])
dockerfile = path.read_text()
old = 'COPY package.json ./\nRUN npm install --registry=https://registry.npmmirror.com'
new = 'COPY package.json package-lock.json ./\nRUN npm ci --no-audit --no-fund'
if dockerfile.count(old) != 1:
    raise SystemExit('upstream Dockerfile dependency step changed')
path.write_text(dockerfile.replace(old, new))
PY

docker build --tag jarvis-9router:0.5.75-runpod-5 "$source_dir"
