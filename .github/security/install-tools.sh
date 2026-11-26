#!/bin/bash
# Installs the security scanners on a GitHub-hosted runner (ubuntu-latest,
# x86_64), in place of ci-runner's slot image. Same tools, same pins, same
# sha256s as ci-runner's slot/Dockerfile.
#
#   install-tools.sh                     gitleaks osv shai-hulud semgrep zizmor
#   install-tools.sh gitleaks            only these
#   install-tools.sh sfw                 Socket Firewall, and npm/npx/pnpm through it
#
# Every download lands in a temp file and is moved into place only after its
# sha256 matches, so a tampered or truncated download never reaches PATH.
# In a job, the bin directory goes on GITHUB_PATH and SHAI_HULUD on GITHUB_ENV,
# so later steps find everything. Outside CI it prints the export lines.
#
# A bump changes a version and its sha256 together. Each pin was the newest
# release at least 14 days old when chosen, so a bad release has had time to be
# noticed. shai-hulud-detect has no releases, so it is pinned by commit.
set -euo pipefail

ALL_TOOLS="gitleaks osv shai-hulud semgrep zizmor"
KNOWN_TOOLS="$ALL_TOOLS sfw"
dest=${SECURITY_TOOLS_DIR:-${RUNNER_TEMP:-/tmp}/security-tools}
bin=$dest/bin
shim=$dest/sfw-shim
here=$(cd "$(dirname "$0")" && pwd)

GITLEAKS_VERSION=8.30.1
GITLEAKS_SHA256=551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb
OSV_VERSION=2.7.0
OSV_SHA256=160c6253e2922e9e0a6a9771c5c4c40a4b3c6090b34a3cdd5e80c96cc2e4218b
ZIZMOR_VERSION=1.31.1
ZIZMOR_SHA256=d185f726bdaf92df78273217dfdd24ebcd6b0ef48cf3ce776ff5762b6e138c99
SHAI_COMMIT=5f23f4dda1f3c85db42045023316db988da699f4
SHAI_DETECTOR_SHA256=93f372024f1ce42405cfc0457ea0e93caf7e5a64cc294dbfaabc5782f3130eee
SHAI_PACKAGES_SHA256=740ab5c24d852697a7e889dd448eb641393b65cd52f229d4d6ef1f40bb8902ab
SEMGREP_VERSION=1.181.0
SFW_VERSION=1.15.4
SFW_SHA256=cf9f55b8343e1ef6e6dcccdb09b99c70c0430a22ed7d0079286eccddeaf72670

tools=${*:-$ALL_TOOLS}
for tool in $tools; do
  case " $KNOWN_TOOLS " in
    *" $tool "*) ;;
    *) echo "install-tools: unknown tool '$tool' (known: $KNOWN_TOOLS)" >&2; exit 2 ;;
  esac
done

# The pins are linux x86_64 binaries: what ubuntu-latest and ubuntu-24.04 run.
platform="$(uname -s)-$(uname -m)"
if [ "$platform" != Linux-x86_64 ]; then
  echo "install-tools: pinned for Linux-x86_64 (ubuntu-latest), not $platform" >&2
  exit 2
fi

mkdir -p "$bin"
work=$(mktemp -d); trap 'rm -rf "$work"' EXIT

# fetch URL SHA256 OUT: download, verify, then move into place.
fetch() {
  local url=$1 sha=$2 out=$3 tmp
  tmp=$work/$(basename "$out").part
  curl -fsSL --retry 3 --proto '=https' -o "$tmp" "$url"
  if ! echo "$sha  $tmp" | sha256sum -c --quiet - >/dev/null 2>&1; then
    echo "install-tools: sha256 mismatch for $url" >&2
    echo "  wanted $sha" >&2
    echo "  got    $(sha256sum "$tmp" | cut -d' ' -f1)" >&2
    exit 1
  fi
  mv "$tmp" "$out"
}

install_gitleaks() {
  fetch "https://github.com/gitleaks/gitleaks/releases/download/v${GITLEAKS_VERSION}/gitleaks_${GITLEAKS_VERSION}_linux_x64.tar.gz" \
    "$GITLEAKS_SHA256" "$work/gitleaks.tgz"
  tar -xzf "$work/gitleaks.tgz" -C "$bin" gitleaks
}

install_osv() {
  fetch "https://github.com/google/osv-scanner/releases/download/v${OSV_VERSION}/osv-scanner_linux_amd64" \
    "$OSV_SHA256" "$bin/osv-scanner"
  chmod +x "$bin/osv-scanner"
}

install_zizmor() {
  fetch "https://github.com/zizmorcore/zizmor/releases/download/v${ZIZMOR_VERSION}/zizmor-x86_64-unknown-linux-gnu.tar.gz" \
    "$ZIZMOR_SHA256" "$work/zizmor.tgz"
  tar -xzf "$work/zizmor.tgz" -C "$bin" zizmor
}

install_shai_hulud() {
  local d=$dest/shai-hulud-detect base="https://raw.githubusercontent.com/Cobenian/shai-hulud-detect/$SHAI_COMMIT"
  mkdir -p "$d"
  fetch "$base/shai-hulud-detector.sh" "$SHAI_DETECTOR_SHA256" "$d/shai-hulud-detector.sh"
  fetch "$base/compromised-packages.txt" "$SHAI_PACKAGES_SHA256" "$d/compromised-packages.txt"
  chmod +x "$d/shai-hulud-detector.sh"
  export_env SHAI_HULUD "$d/shai-hulud-detector.sh"
}

# Its own venv, so its dependencies never meet the repo's.
install_semgrep() {
  python3 -m venv "$dest/semgrep-venv"
  "$dest/semgrep-venv/bin/pip" install --no-cache-dir -q "semgrep==${SEMGREP_VERSION}"
  ln -sf "$dest/semgrep-venv/bin/semgrep" "$bin/semgrep"
}

# Socket Firewall, and the shim that sends npm, npx and pnpm through it. Licence:
# PolyForm Shield, free to use, not for building a competing product.
install_sfw() {
  fetch "https://github.com/SocketDev/sfw-free/releases/download/v${SFW_VERSION}/sfw-free-linux-x86_64" \
    "$SFW_SHA256" "$bin/sfw"
  chmod +x "$bin/sfw"
  mkdir -p "$shim"
  cp "$here/sfw-shim.sh" "$shim/sfw-shim.sh"
  chmod +x "$shim/sfw-shim.sh"
  local n; for n in npm npx pnpm; do ln -sf sfw-shim.sh "$shim/$n"; done
  # sfw only sees what reaches the network; with a warm cache a flagged package
  # in the lockfile would install unseen (ci-runner's case study, 2026-11-01).
  export_env npm_config_prefer_online true
  shim_on_path=1
}

envs=()
export_env() { envs+=("$1=$2"); }
shim_on_path=0

for tool in $tools; do
  echo "install-tools: $tool"
  "install_${tool//-/_}"
done

if [ -n "${GITHUB_PATH:-}" ]; then
  # Each line is prepended, so the shim (written last) ends up first.
  echo "$bin" >> "$GITHUB_PATH"
  [ "$shim_on_path" = 0 ] || echo "$shim" >> "$GITHUB_PATH"
  [ ${#envs[@]} = 0 ] || printf '%s\n' "${envs[@]}" >> "$GITHUB_ENV"
else
  echo "export PATH=\"$( [ "$shim_on_path" = 0 ] || printf '%s:' "$shim" )$bin:\$PATH\""
  [ ${#envs[@]} = 0 ] || printf 'export %s\n' "${envs[@]}"
fi
