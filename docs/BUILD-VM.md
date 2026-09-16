# Private build runtime

The Codex bridge can execute allowed Cargo/npm/npx commands in disposable KVM
guests. Builds receive a source snapshot, read-only dependencies and loopback
networking and a read-only public dependency gateway. They do not receive host
credentials or an external network device.
Original Write hooks and concurrent-edit checks apply when source changes return.

## Configuration

The daemon discovers `~/.local/share/augmentagent/build-vm/runtime.json` by
default. Set `AUGMENTAGENT_BUILD_VM_CONFIG` in the daemon environment to override
it. An explicit invalid path fails; it does not fall back to the default.
Task/profile environment variables cannot select a different runtime.

Keep the directory mode 0700 and configuration mode 0600, owned by the daemon
user. All paths must be absolute. Host executable artifacts must be owned by root
or the daemon user and must not be group/world writable. Example schema (replace
every example path with a provisioned local artifact):

```json
{
  "qemu": "/opt/private-build-vm/usr/bin/qemu-system-x86_64",
  "kernel": "/opt/private-build-vm/boot/vmlinuz-example",
  "busybox": "/bin/busybox",
  "firmware": "/opt/private-build-vm/usr/share/seabios",
  "data_dir": "/opt/private-build-vm/usr/share/qemu",
  "library_dir": "/opt/private-build-vm/usr/lib/x86_64-linux-gnu",
  "module_dir": "/opt/private-build-vm/usr/lib/x86_64-linux-gnu/qemu",
  "modules": [
    "/opt/private-build-vm/kernel-modules/netfs.ko",
    "/opt/private-build-vm/kernel-modules/9pnet.ko",
    "/opt/private-build-vm/kernel-modules/9pnet_virtio.ko",
    "/opt/private-build-vm/kernel-modules/9p.ko"
  ],
  "memory_mb": 4096,
  "toolchain": "/opt/private-rust-toolchain",
  "registry": "/opt/private-cargo-cache/registry"
}
```

Provision QEMU with KVM, virtio-9p and seccomp support, a compatible Linux kernel,
its matching uncompressed modules in dependency order, and a static BusyBox.
The module list depends on which drivers the kernel includes; virtio PCI must be
available before loading 9p. The daemon user needs access to `/dev/kvm`.
On Debian/Ubuntu, distribution packages and their shared-library dependencies
can be extracted into a private directory using `dpkg-deb -x`; system installation
is not required. Keep package versions and hashes in a private provisioning record.
See the [QEMU command documentation](https://www.qemu.org/docs/master/system/qemu-manpage.html).

The guest mounts host `/usr` read-only for compiler and Python userspace. Optional
`toolchain`, `registry` and `cargo_git` directories supply read-only Rust dependencies.
Explicit offline flags remain supported. npm dependencies come from root and nested workspace
`node_modules` directories, mounted read-only at their original relative paths.
For npm installation commands, the bridge copies available dependencies into a
private writable snapshot, then runs npm and lifecycle scripts only in the guest.
Successful installations are retained privately for the bridge's lifetime and
mounted read-only for subsequent build/test commands. Resolution-input changes
invalidate that copy; closing the bridge removes it. Generated executables never
replace the host checkout's installed dependencies. File copies reject devices
and symlinked roots, preserve internal links as guest data, and limit each tree to
100,000 entries and 2 GiB. Local packages and uncached public registry packages are supported. Native Node
addons use the matching read-only system headers and build from source in the
guest. Provision the Node headers and C/C++ compiler alongside Node.

Fresh linked Git worktrees can reuse installed dependencies from their registered
main checkout when the requested worktree has no installed dependency roots.
Lockfile bytes and dependency declarations must match; test/build script edits
are allowed. Nested independent packages require their own matching lockfile;
workspaces may use the root lock. Unrelated or unlocked installs are excluded.
The main checkout stays outside model file access, and its dependency mounts
remain read-only. This reuses installed packages; it does not download missing
packages or execute host installation scripts. New packages use the gateway below.

Discovery skips control/build directories, refuses symlinked dependency roots,
and permits at most 16 mounts per build. Destination traversal, duplicate mounts
and symlinked mount destinations are rejected before launching a guest. Memory must be 512–4096 MiB; guests
use two virtual CPUs. A configuration file alone does not prove runtime readiness.
The guest supervisor collects stdout and stderr through pipes with a combined
8 MiB cap, terminating an overproducing command while it is still running. Output
is not spooled to unbounded guest files. The host accepts a result only after VM
shutdown and verified supervisor cleanup.

## Verification and rollback

Run the real isolation and bridge contracts with the provisioned configuration:

```sh
export JARVIS_TEST_VM_CONFIG="$HOME/.local/share/augmentagent/build-vm/runtime.json"
python3 scripts/tests/codex_build_vm_test.py
python3 scripts/tests/codex_tool_bridge_test.py
cargo test -p augmentagent-channel-core live_codex_builds_and_tests_with_the_vm_bridge --lib -- --ignored
```

The last command needs working Codex authentication and uses the daemon's default
configuration selection (or its explicit override). It verifies actual broker
execution, a passing Cargo socket test and the tool audit.

Installing runtime files does not deploy or restart the daemon. Retain the previous
binary and runtime configuration for rollback, including private provider cooldown
and handoff state. Restore the prior binary/configuration and restart through the
normal service procedure. Do not delete handoff state to force a retry. Before
rolling back to a version that cannot read the new handoff journals, pause task
intake and scheduled mutation workflows. Reconcile in-flight/uncertain operations
using the current recovery tooling before resuming them; an older binary cannot
safely infer that an unrecognized receipt means an operation did not occur.
Verify saved binary hashes and configuration permissions before replacement,
then verify the running executable and service health after restart.

Build artifacts and writable package caches are private to a bridge session.
Disk quotas are not supplied by this runtime; provision host capacity separately. Binary source updates and file deletions reconcile in
the source-build profile; profiles with text-only Write hooks reject those
changes explicitly. Full fallback rollout and controlled auto-ship acceptance
must be verified separately.


## Public dependency gateway

The guest resolves `registry.npmjs.org`, `index.crates.io` and `static.crates.io`
to its own loopback HTTPS server. A fresh, guest-only certificate preserves the
canonical registry URLs and cache identities; the signing key is under the
guest's root-only directory. This does not change the host trust store. The host
needs its trusted OpenSSL executable to prepare that certificate.

The gateway permits GET/HEAD requests and relays only validated package paths to
those fixed HTTPS origins. A separate host child retrieves each response with an
empty credential/proxy environment; it never runs package code. Other HTTP
methods, origins, traversal and query injection are denied. Fetches share the
build deadline and are bounded to 20 seconds per request, 32 MiB per response,
512 MiB total and 2,048 requests per invocation. The guest has no external NIC.
Private registries and arbitrary Git dependency hosts require provisioned caches;
the gateway does not forward authentication credentials.

Cargo uses a private writable copy of the provisioned registry/git cache when
fetching, preserves canonical crates.io identities, and can reuse newly downloaded
crates on a subsequent offline build. Those copies never reconcile into source or
replace the operator's cache. Session reuse keeps registry archives and index data,
not extracted source directories or guest-modified Git checkouts. Cargo re-extracts
archives on each invocation; Git dependencies start from the provisioned operator
cache. Explicit `--offline` and `--frozen` flags retain their offline behavior.
The executable toolchain directory must be owned by
root or the daemon user and must not be group/world writable; provision a private
copy when the ordinary Rust installation uses shared permissions.

Verification now includes uncached npm and Cargo installs, an offline Cargo
rebuild, denied direct networking and HTTP mutations, inaccessible signing keys,
canonical lockfiles and a fresh npm-ci session. A clean worktree of this project
passed installation, build and all 26 Node tests through the gateway, including
compilation of its native SQLite addon.
