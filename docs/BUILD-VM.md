# Private build runtime

The Codex bridge can execute allowed Cargo/npm/npx commands in disposable KVM
guests. Builds receive a source snapshot, read-only dependencies and loopback
networking. They do not receive host credentials or an external network device.
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
Commands run offline. npm dependencies come from root and nested workspace
`node_modules` directories, mounted read-only at their original relative paths.
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
normal service procedure. Do not delete handoff state to force a retry.

Cache reuse, missing dependency provisioning and disk resource limits remain
incomplete. Binary source updates and file deletions reconcile in
the source-build profile; profiles with text-only Write hooks reject those
changes explicitly. Full fallback rollout and controlled auto-ship acceptance
must be verified separately.
