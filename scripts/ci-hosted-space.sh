#!/usr/bin/env bash
# Sourced (`. scripts/ci-hosted-space.sh`) so its TMPDIR reaches the tests.

# Snapshot/recovery tests retain the production free-space reserve. The hosted
# Ubuntu image includes large mobile/.NET/Haskell/CodeQL SDKs this Rust job
# never uses. Targeted runs retain several crates' test executables, so reclaim
# these before compilation as well as preserving the snapshot reserve itself.
# Never remove host software when this command is replayed by local CI.
if [[ "${GITHUB_ACTIONS:-}" != true || "${RUNNER_ENVIRONMENT:-}" != github-hosted || "${RUNNER_OS:-}" != Linux ]]; then
  return 0 2>/dev/null || exit 0
fi

df -h .
sudo rm -rf -- /usr/local/lib/android /usr/share/dotnet \
  /opt/ghc /usr/local/.ghcup /opt/hostedtoolcache/CodeQL
df -h .

# Even after that, the restored cache plus the test build leave the root disk
# about 7.6 GB free, below the 15 GB snapshot reserve the recovery tests keep
# (main red at c1b60b2, 2026-11-05). Hosted runners carry a second, mostly
# empty disk at /mnt: put test scratch there instead of lowering the reserve.
if [[ -d /mnt ]] && scratch_free=$(df -Pk /mnt | awk 'NR==2 {print $4}') && (( scratch_free > 20 * 1024 * 1024 )); then
  sudo install -d -m 1777 /mnt/ci-tmp
  export TMPDIR=/mnt/ci-tmp
  df -h /mnt
fi
