#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

# Keep unrelated optional native packages out of the voice installation. DAVE
# needs exactly one platform binding, which npm otherwise omits with .npmrc.
case "$(uname -s):$(uname -m)" in
  Darwin:arm64) codec='darwin-arm64' ;;
  Darwin:x86_64) codec='darwin-x64' ;;
  Linux:x86_64) codec='linux-x64-gnu' ;;
  Linux:aarch64) codec='linux-arm64-gnu' ;;
  *) echo 'unsupported Discord voice host architecture' >&2; exit 1 ;;
esac

npm ci
npm install --no-save --no-package-lock --omit=optional --ignore-scripts \
  "@snazzah/davey-${codec}@0.1.12"
./node_modules/node/bin/node -e "require('@snazzah/davey')"
npm run build
