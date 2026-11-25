#!/usr/bin/env bash
# Hold the cache lock inside the container, including artifact capture. Thus
# disconnecting SSH cannot release a surviving compiler's cache exclusion.
container_build() (
  set -euo pipefail
  local toolchain="$1" work="$2" cache="$3" output="$4"
  [[ "$toolchain" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || exit 75
  exec 8>"$cache/build.lock"
  flock 8
  [ "$(df -Pk "$cache" | awk 'NR==2 {print $4}')" -gt 41943040 ] || exit 73
  # Git archives preserve OLD commit timestamps. Refresh them after taking the
  # cache lock so Cargo cannot reuse a newer commit's binary for an ancestor.
  # Rust incremental/codegen and registry caches remain persistent.
  find "$work" -type f -exec touch {} +
  cd "$work"
  export CARGO_HOME="$cache/cargo" CARGO_TARGET_DIR="$cache/target"
  cargo "+$toolchain" build --release --locked --target x86_64-unknown-linux-gnu \
    -p augmentagent-cli -p augmentagent-mcp-memory
  for name in augmentagent augmentagent-mcp-memory; do
    artifact="$cache/target/x86_64-unknown-linux-gnu/release/$name"
    [ -f "$artifact" ] && [ ! -L "$artifact" ] || exit 75
    install -m 755 "$artifact" "$output/$name"
  done
)
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  container_build "${1:-}" /work /cache /output
fi
