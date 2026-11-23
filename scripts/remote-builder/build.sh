#!/usr/bin/env bash
# Only dispatch.py calls this root-owned program; SHA is the entire input.
remote_builder_build() (
set -euo pipefail
sha=${1:-}
[[ "$sha" =~ ^[0-9a-f]{40}$ ]] || exit 75
root="$2"
exec 9>"$root/build.lock"
flock -n 9 || exit 75
# Keep the target and Cargo registry stable across exact-commit builds.
# Admission is serialized, and no source/Dockerfile supplied by the key is run
# on the host. Only GitHub main's history is accepted.
git --git-dir="$root/repo.git" fetch --no-tags origin "$sha" main
[ "$(git --git-dir="$root/repo.git" rev-parse 'FETCH_HEAD^{commit}')" = "$sha" ] || {
  # Fetching two refs leaves FETCH_HEAD order unspecified; verify the object
  # itself, then prove custody in the separately fetched main ref below.
  [ "$(git --git-dir="$root/repo.git" rev-parse "$sha^{commit}")" = "$sha" ]
}
git --git-dir="$root/repo.git" fetch --no-tags origin main:refs/heads/approved-main
git --git-dir="$root/repo.git" merge-base --is-ancestor "$sha" refs/heads/approved-main
# Reserve space for rebuildable output rather than consuming the whole box.
[ "$(df -Pk "$root" | awk 'NR==2 {print $4}')" -gt 41943040 ] || exit 73
source_dir=$(mktemp -d "$root/source.XXXXXX")
stage=$(mktemp -d "$root/artifacts/.stage.XXXXXX")
trap 'rm -rf "$source_dir" "$stage"' EXIT
git --git-dir="$root/repo.git" archive "$sha" | tar -xf - -C "$source_dir"
toolchain=$(python3 -c 'import sys,tomllib;print(tomllib.load(open(sys.argv[1],"rb"))["toolchain"]["channel"])' "$source_dir/rust-toolchain.toml")
[[ "$toolchain" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || exit 75
# This image is provisioned by setup.sh, not built from downloaded repository code.
docker run --rm --platform linux/amd64 --read-only --user "$(id -u):$(id -g)" \
  --cpus 6 --memory 24g --pids-limit 512 --cap-drop ALL --security-opt no-new-privileges \
  --tmpfs /tmp:rw,nosuid,nodev,size=2g \
  --mount "type=bind,src=$source_dir,dst=/work" \
  --mount "type=bind,src=$root/cache,dst=/cache" \
  --mount "type=bind,src=$stage,dst=/output" \
  --workdir /work --env HOME=/tmp --env CARGO_HOME=/cache/cargo \
  --env CARGO_TARGET_DIR=/cache/target --env CARGO_BUILD_JOBS=4 \
  --env CARGO_PROFILE_RELEASE_INCREMENTAL=true --env CARGO_PROFILE_RELEASE_DEBUG=0 \
  "jarvis-release-builder:$toolchain" \
  /usr/local/bin/jarvis-container-build "$toolchain"
python3 - "$source_dir" "$stage" "$sha" "$toolchain" <<'PY'
import hashlib,json,pathlib,sys,tomllib
source,stage=map(pathlib.Path,sys.argv[1:3])
version=tomllib.load(open(source/'Cargo.toml','rb'))['workspace']['package']['version']
checksums={}
for name in ('augmentagent','augmentagent-mcp-memory'):
 h=hashlib.sha256()
 with (stage/name).open('rb') as stream:
  for block in iter(lambda:stream.read(1024*1024),b''):h.update(block)
 checksums[name]=h.hexdigest()
(stage/'manifest.json').write_text(json.dumps({'commit':sys.argv[3],'toolchain':sys.argv[4],
 'version':version,'target':'x86_64-unknown-linux-gnu','sha256':checksums})+'\n')
PY
# A commit's published directory is immutable, so a slow concurrent download
# cannot see files from a later build. Failed builds never publish artifacts.
if [ ! -d "$root/artifacts/$sha" ]; then
  mv "$stage" "$root/artifacts/$sha"
fi
printf 'remote build complete: %s\n' "$sha"

)
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  remote_builder_build "${1:-}" /srv/jarvis-remote-builder
fi
