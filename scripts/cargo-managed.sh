#!/usr/bin/env bash
# Session builds share admission, ownership discovery, and prune exclusion.
set -euo pipefail
repo_root=$(git rev-parse --show-toplevel)
git_common=$(git rev-parse --path-format=absolute --git-common-dir)
cli="${AUGMENTAGENT_STORAGE_BIN:-$(dirname "$git_common")/target/release/augmentagent}"
if [ ! -x "$cli" ]; then
  echo "Set AUGMENTAGENT_STORAGE_BIN to the deployed augmentagent binary" >&2
  exit 1
fi
if [ -z "${CARGO_TARGET_DIR:-}" ]; then
  cache_home="${XDG_CACHE_HOME:-$HOME/.cache}"
  case "$cache_home" in /*) ;; *) cache_home="$HOME/.cache" ;; esac
  default_parent="$cache_home/augmentagent-session-targets"
  if [ -d /mnt/build ] && [ -w /mnt/build ]; then
    default_parent=/mnt/build/augmentagent-session-targets
  fi
  parent="${AUGMENTAGENT_BUILD_PARENT:-$default_parent}"
  worktree_key=$(printf '%s' "$repo_root" | sha256sum | cut -c1-16)
  "$cli" disk manage-parent --root "$parent"
  CARGO_TARGET_DIR="$parent/$(basename "$repo_root")-$worktree_key"
fi
exec "$cli" disk build --target-dir "$CARGO_TARGET_DIR" -- cargo "$@"
