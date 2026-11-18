#!/usr/bin/env bash
# Reproducible OFFLINE-FROM-PRODUCTION builder setup (root, Debian 13).
# Owner runs after reviewing this PR, on the idle GalaxyGate box, NOT Legion:
#   sudo bash scripts/remote-builder/setup.sh /path/to/updater-builder-key.pub
#
# Create a dedicated SSH key on the deploying host and verify/pin the builder's
# host key out of band. Then add ONLY these env vars to the updater unit:
#   AUGMENTAGENT_REMOTE_BUILD_HOST=builder@204.235.251.13
#   AUGMENTAGENT_REMOTE_BUILD_IDENTITY=/absolute/path/to/private-key
#   AUGMENTAGENT_REMOTE_BUILD_TIMEOUT=3600            # optional seconds
# Unset HOST retains the existing local build. Remote mode never falls back.
# Every remote failure returns 75; 73/75/76 at run_update_build withhold the
# stamp. A later tick retries. Local behavior and secure artifact repair stay.
#
# Protocol: forced `build <40-hex-sha>`; `scp -O` can READ ONLY
# <sha>/{manifest.json,augmentagent,augmentagent-mcp-memory}. SFTP, uploads,
# shells, PTY and forwarding are denied. No Legion config is changed here.
# The key's build permission executes protected main's Cargo build scripts in
# a constrained Ubuntu 24.04 amd64 container. The dedicated box is the trust
# boundary; do not share it with production services or mount the Docker socket.
#
# Source: exact requested SHA fetched from fixed GitHub origin; only main's
# history accepted. Image: Ubuntu 24.04/glibc 2.39, Rust pinned by the checkout's
# rust-toolchain.toml. Cache: /srv/jarvis-remote-builder/cache, builds serialized.
# Old commit artifacts are immutable; prune old artifact dirs manually only
# when no download is active. Cargo caches are never pruned during a build.
# For a toolchain bump, rerun this reviewed setup from that toolchain's checkout.
#
# Compatibility dry run AFTER setup: request a known main SHA with the updater
# key, pull all three files with scp -O, then use an ubuntu:24.04 amd64 container
# to verify checksums, `augmentagent --version`, and memory-server EOF with a
# disposable AUGMENTAGENT_DB. client.py performs checks/loading on Legion before
# atomic per-file install; never run an unverified binary from a deployment DB.
set -euo pipefail
[ "$(id -u)" -eq 0 ] || { echo 'run as root on the dedicated builder' >&2; exit 1; }
[ "$#" -eq 1 ] || { echo 'usage: setup.sh UPDATER_PUBLIC_KEY' >&2; exit 1; }
base=$(cd "$(dirname "$0")" && pwd)
apt-get update
apt-get install -y --no-install-recommends docker.io git openssh-server python3 util-linux
# Require a single plain key; reject injected authorized_keys options/newlines.
key=$(python3 - "$1" <<'PY'
import base64,pathlib,sys
line=pathlib.Path(sys.argv[1]).read_text().strip()
parts=line.split()
if '\n' in line or len(parts)<2 or parts[0] not in ('ssh-ed25519','ssh-rsa'):
 raise SystemExit('expected one plain SSH public key')
base64.b64decode(parts[1],validate=True)
print(parts[0]+' '+parts[1])
PY
)
ssh-keygen -l -f "$1" >/dev/null
id builder >/dev/null 2>&1 || useradd --create-home --shell /bin/bash builder
usermod -aG docker builder
systemctl enable --now docker
install -d -o builder -g builder -m 700 /home/builder/.ssh
printf 'restrict,command="/usr/local/lib/jarvis-remote-builder/dispatch.py" %s\n' "$key" \
  > /home/builder/.ssh/authorized_keys
chown builder:builder /home/builder/.ssh/authorized_keys
chmod 600 /home/builder/.ssh/authorized_keys
install -d -m 755 /usr/local/lib/jarvis-remote-builder
install -m 755 "$base/build.sh" "$base/dispatch.py" /usr/local/lib/jarvis-remote-builder/
root=/srv/jarvis-remote-builder
install -d -o builder -g builder -m 700 "$root" "$root/cache" "$root/artifacts"
if [ ! -d "$root/repo.git" ]; then
  runuser -u builder -- git init --bare "$root/repo.git"
  runuser -u builder -- git --git-dir="$root/repo.git" remote add origin https://github.com/nolanmak/Jarvis.git
fi
toolchain=$(python3 -c 'import sys,tomllib;print(tomllib.load(open(sys.argv[1],"rb"))["toolchain"]["channel"])' "$base/../../rust-toolchain.toml")
[[ "$toolchain" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || exit 1
docker build --platform linux/amd64 --build-arg "TOOLCHAIN=$toolchain" \
  -t "jarvis-release-builder:$toolchain" "$base"
echo 'Builder provisioned. Review/pin host key, dry-run compatibility, then opt in on updater separately.'
