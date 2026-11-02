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

### Runner selection and readiness (#1041)

The daemon resolves one build runner for Codex `cargo`, `npm` and `npx` commands:

| Daemon environment | Runner |
| --- | --- |
| `AUGMENTAGENT_BUILD_VM_CONFIG=<path>` | `vm` (an invalid path fails; the opt-out does not override it) |
| `AUGMENTAGENT_BUILD_VM=host` | `host` (explicit opt-out: the host command sandbox) |
| default `runtime.json` present | `vm` |
| none of the above | unavailable |

There is no silent host fallback. When the runner is unavailable, every build
command returns the tool error `JARVIS_READINESS:build_vm_unavailable` and nothing
runs; other commands are unaffected. The daemon logs a warning at launch when a
build-capable profile starts without a runner.

Every `Bash` row in `tool-audit.log` carries `runner`: `vm` or `host` for a command
whose process started, `none` for one refused before any process started (a
readiness error, a policy denial, or a Claude permission refusal). The bridge
decides the runner before execution and reports it as the first key of the
result, or as a `[runner=vm]` / `[runner=host]` prefix on a failure raised after
the process started (for example a timeout), so truncation cannot drop it.
Non-build commands and every Claude-lane command run on the host.

`augmentagent doctor` reports the `build_vm` check as one of: `config missing`,
`config invalid`, `qemu or kernel missing`, `kvm not accessible` (error: no
read-write access, or no device), `kvm not accessible after logout` (warning:
access comes only from a per-user ACL entry such as the login-seat grant, not
from the owner, the `kvm` group entry under the ACL mask, or other bits), or `ok`.
With the opt-out set it warns that builds run on the host.

### Build scratch, persistence and timeout (#1036)

Rust dependencies can use native artifacts outside Cargo's registry. In
particular, `ort-sys` downloads ONNX Runtime during its build script. The build
guest has no external network interface, so the operator must provision this
dependency before the build. An optional `ort_cache` field in the private runtime
JSON names an absolute, literal directory containing the `ort-sys` cache layout:
`dfbin/<target>/<distribution-sha256>/libonnxruntime.a`. Provision the distribution
required by the locked `ort-sys` version using its normal integrity-checked
download on the operator side. This is an explicit dependency export, never a
discovery or export of the operator's whole home/cache directory.

The guest mounts only that directory read-only at `/ort-cache` and sets
`ORT_CACHE_DIR=/ort-cache`. Tasks cannot supply this environment variable. The
selected distribution must already exist; a missing distribution still fails
the build. This does not disable linking, substitute a stub, add a network
device, or extend the package gateway's permitted origins. Existing runtime
declarations without `ort_cache` retain their behavior.

Every VM build file lives under the scratch root, never the root disk:
`AUGMENTAGENT_BUILD_SCRATCH_DIR` in the daemon environment, default
`/mnt/build/codex-vm`. Codex runs with a cleared environment, so the daemon
passes the root to the bridge in its policy (`build_scratch_dir`). A root inside a
model-writable directory (resolved through its nearest existing ancestor) is withheld
from the policy, so only build commands become unavailable while other tools keep
working. The root must be a real directory (not a symlink), owned by the daemon user,
with mode exactly 0700; the bridge checks this on an `O_NOFOLLOW` descriptor and creates
sessions relative to it. Otherwise every VM build fails closed with
`JARVIS_READINESS:build_scratch_unavailable` naming the path, and nothing runs on the
host. Provision it once:

```sh
install -d -m 700 /mnt/build/codex-vm
```

Each bridge session creates `<root>/jarvis-vm-session-*` holding:

- `owner.json`: the bridge pid and its kernel start time.
- `build-cache.img`: a sparse ext4 image capped at `AUGMENTAGENT_BUILD_SCRATCH_IMAGE_CAP_GIB`
  (12 GiB by default), mode 0600, attached to each
  `cargo` guest as a virtio disk at `/build-cache`. It holds the Cargo target
  directory and Cargo home for the whole session, so a second `cargo` command is
  incremental. Snapshots keep source mtimes. The host never mounts it. It is a disk
  image, not a 9p share, because the guest's 9p client stores timestamps to the
  second, and Cargo then treats every dependency as rebuilt. On first use the guest
  seeds the Cargo home from the read-only operator registry and Git cache mounts.
  Nothing is copied on the host, and the registry is never copied per command.
  12 GiB covers one checkout's debug target for a couple of workspace crates
  (the burn-down target with the channel-core and cli test builds is 9.4 GiB) plus
  about 0.8 GiB of Cargo home.
- `tmp/`: each command's source snapshot, VM control files and initrd, and the
  session's npm install copy.

Disk safety: the scratch volume is shared with the daemon's own build caches.
A new session is admitted only when both of these hold:

- Free space covers the whole new image, the unallocated remainder of every
  other session's image, and the free-space headroom.
- The allocated blocks of all images, plus the new image's cap, stay within the
  build-cache budget.

The three admission limits are configurable from the **daemon environment only**
(never a task/profile environment or the model), carried to the bridge in its
policy exactly like the scratch root. All are integer GiB:

| Daemon environment | Default | Lower bound | Meaning |
| --- | --- | --- | --- |
| `AUGMENTAGENT_BUILD_SCRATCH_HEADROOM_GIB` | 20 | 4 | free space kept after admitting a session |
| `AUGMENTAGENT_BUILD_SCRATCH_IMAGE_CAP_GIB` | 12 | 10 | per-session build-cache image cap |
| `AUGMENTAGENT_BUILD_SCRATCH_BUDGET_GIB` | 24 | ≥ image cap | total build-cache budget across sessions |

On this host the defaults admit one session at a time: the 24 GiB budget would
by itself fit two 12 GiB images, but the free-space rule is the binding
constraint — the first session needs 32 GiB free (20 GiB headroom + a 12 GiB
image) and a second concurrent session needs about 44 GiB, more than the
volume's ~37 GiB free. Raise the budget and free more space to run two. A value
that is out of range or unparseable fails closed with
`JARVIS_READINESS:build_scratch_limits` before any build-scratch session or
image file is created — it is never silently clamped. `augmentagent doctor`
reports the `build_scratch` check as the scratch volume's free space, the three
effective limits, and how many sessions can be admitted right now (an error when
the limits are invalid). Doctor reads these from its own process environment, so
run it with the same environment the daemon has (e.g. the same `.env`); limits
set only via the service's `Environment=` will not show unless doctor is run
with them too. Because the scratch volume is shared with the auto-PR
gate cache, raising `AUGMENTAGENT_GATE_CACHE_MAX_MB` leaves less free space for
build sessions; lower it (or the headroom) if the two contend on this one host.

Otherwise builds fail with `JARVIS_READINESS:build_scratch_space`, naming the
path and the numbers, before any build-scratch session or image file is created.
The image is attached with
`werror=report,rerror=report`, so a host ENOSPC fails the guest's writes instead
of pausing the VM until its deadline. When the image or the volume fills, the
build reports `JARVIS_READINESS:build_cache_full` naming the image or the scratch
root.

Closing the bridge removes the session. A bridge killed without cleanup (the
reasoner watchdog kills the whole process group) leaves its session behind. The
daemon's hourly sweep loop, which also runs at start, removes sessions whose owner
pid and start time no longer match a running process. Before removing a session
it kills that session's VM processes. A process is one only if its executable is
`qemu-system-*` or `python3` (the supervisor) and one of its arguments starts with
the session path or is a qemu `path=`/`file=` option inside it. A shell that merely
mentions the path is never killed. Each kill goes through `pidfd_open` and
re-checks the start time first, so a reused pid is never signalled. Codex starts
the bridge with a plain `Command` in a new process group, not a pid namespace, so
the bridge's own pid is the one the daemon sees.

A `cargo`, `npm` or `npx` command with no explicit `timeout` gets
`AUGMENTAGENT_BUILD_TIMEOUT_SECS` (default 600 s, the longest a Claude-lane Bash
call runs), passed as `build_timeout_secs` and capped at the tool maximum of
900 s. Other commands default to 120 s. An explicit timeout is honoured.

### Shared CI input capture (#1807)

Inactive external dependency producer payloads have a separate retirement path
inside the admitted cache image. The original host session must release its
source lifetime and supply its original verified producer acknowledgement.
Another session's unprotected producer is not eligible merely because its ID is
absent from the current session. Cleanup removes at most two eligible producers
per command. It records original control/publication metadata and retirement
authorization durably before deletion, and can resume a committed interrupted
retirement under normal storage admission. The physical cache must still match.
Published seeds and installed hard links remain intact during producer cleanup.
Unknown ownership is preserved; recovery of unreleased producers across process
death remains unfinished.

Before workers start, the same admitted controller can reclaim at most two
orphaned published seeds. It inventories all current indexes, allocated source
generations and producer controls, and protects the current installation request.
An absent producer alone is insufficient: reclamation requires its original
completed retirement and matching publication/host acknowledgement. The existing
storage lease remains held across seed lookup and consumption. Current indexed
seeds and installed generations stay warm. Unknown references, extra files,
symlinks, foreign cache identities and incomplete inventories retain the payload.
An original manifest is committed before unlinking; interrupted deletion resumes
only from that matching private intent. Host audit records the reclaimed seed
identities after original VM quiescence. This bounds orphan cleanup per command;
global retention across distinct current dependency indexes and bounded history
retention remain unfinished. This is not recovery of the full legacy image.

Shared CI runs each worker command through the existing descendant supervisor
inside the guest. The controller retains its cleanup receipt under the guest's
private `/root/ci-worker-control`, outside the worker's writable mounts. It
requires a stopped supervisor and a valid receipt before capturing generated
inputs. Detached children are stopped and reaped before capture. The host still
requires its separate whole-VM quiescence and storage-settlement evidence.

The controller starts a bounded write journal before launching that worker and
records content/mode snapshots of declared generated directories and files.
Restoring earlier bytes or timestamps does not hide an observed write. A bound
journal cannot finish with another worker scope's cleanup receipt. Internal
aliases may resolve only through declared namespaces; escaped, cyclic, special,
lost-watch and overflowed observations remain typed prerequisites. Reads alone
do not invalidate the observation.

These observations are partial evidence. They do not establish complete compiler
inputs inside build outputs, artifact provenance or compatible-unit invalidation.
The current local-completion producer remains `preparation_only`, with
`reusable_passing_proof: false`. A passing local command or input observation
does not authorize merge; final proof publication and merge enforcement remain
separate required work.

### Candidate protected Cargo planner (#1807, not enabled)

The [planner proposal](ci-cargo-planner.md) records the actual guest request and
fingerprint observations, proposed unit identity/cleanup rules and the remaining
blocking questions. It does not enable the mode or hand off a final C contract.

This is a candidate mechanism under interrogation. The capability probe and an unprivileged native exec-denial primitive are
implemented; protected Cargo execution, artifact invalidation and the final
passing-proof producer are not. This section does not authorize a rollout or a
consumer handoff. All #1807 D1–D8 requirements, including compatible-unit reuse,
remain binding. C owns resolving the open threads below before implementation
acceptance. An unsupported premise rejects the candidate; it is not permission
to change the operator's toolchain channel, provider grant or VM restrictions.

1. **Protect the planner's metadata.** The pinned Cargo executable would run under
   a dedicated non-root planner credential inside the existing VM, distinct from
   both the controller and application credentials, with its fingerprint metadata
   inaccessible to application workers. Build scripts, procedural macros,
   wrappers, linkers, plugins and tests must execute without that privilege.
   An application-written Cargo fingerprint or dep-info file is not a reuse
   authority. Only independently protected producer records may support reuse.
2. **Prevent application execution under the planner credential.** The current
   prototype direction is a controller-embedded native gate denying the planner's
   direct exec calls, with tool execution delegated to a controller relay under a
   separate unprivileged credential. A preload adapter may translate the pinned
   Cargo/std-library spawn interfaces to that relay; the kernel denial remains
   mandatory if interposition misses a call. A same-UID interface experiment now
   exercises that delegation; the protected relay, credential transition and
   production loader protocol remain unimplemented and unaccepted. Unsupported ABI,
   missing listener, ambiguous caller identity or unknown executable must refuse.
   A capability observation establishes none of this enforcement. Application
   environment/arguments cannot select the launcher, credentials or gate policy.
3. **Close each producer's inputs and outputs.** Before compilation, expose the
   original read-only Git source, frozen declared generated inputs, trusted
   dependency artifacts and the pinned runtime. Separate producer credentials
   and private output paths must prevent other workers from modifying an active
   producer or its result. Capture build-script outputs before dependent rustc
   invocations. Publish and seal a unit only after its descendants are stopped
   through the existing guest supervisor. Whole-VM host quiescence and managed
   storage admission remain independently required; this is not a second host
   storage fence.
4. **Invalidate from protected content identities.** Bind a unit to package,
   Rust target/build-script, target triple, profile, feature/cfg/test mode, flags,
   runtime/toolchain and its complete input closure. Use protected Cargo graph
   data and producer records to invalidate changed units and transitive outputs,
   including build-script execution state. Pure compiler units need precise
   inputs so unrelated generated files do not force recompilation. Native or
   otherwise opaque producers need an explicit complete visible-input closure;
   a candidate-supplied dependency hint cannot make it smaller. Unknown formats
   or incomplete closure must return a typed prerequisite. Every unchanged
   compatible unit must remain reusable, and cross-generation external reuse
   still requires the existing independently produced, sealed seed.
5. **Keep one evidence chain.** Private planner/unit records belong to the owned
   source generation and physical managed image. They are inputs to C's existing
   proof contract, not another workflow binding or CI verdict. Crashes cannot
   promote incomplete records. Source-generation reconstruction, changed
   authority and unknown ownership retain the existing refusal/recovery rules.
   Final proof publication still requires complete local coverage, original
   bindings, input provenance and guest/host cleanup evidence. V/M/baseline
   integration and the D7 rollout/storage-enforcement requirements remain open.

Load-bearing open threads (owner C; each blocks production privilege activation and final
proof publication): the precise launcher/listener credential and loader protocol;
the protected graph/unit record schema and opaque-input closure rule that meets
D8; frozen generated-input and build-output provenance; and bounded/crash-safe
artifact retention using R's existing admission/quiescence. None is an accepted
weaker alternative or a new product requirement.

The first fresh-reader audit identified six unresolved implementation choices:
the trusted graph/input interface, execution/loader protocol, unit-scoped cleanup
bindings, generated-output publication, unreleased-owner recovery, and exact
unit-to-suite proof/API mapping. All remain blockers, not accepted guesses.
Unit cleanup must have its own producer identity and evidence before dependent
compilation; command cleanup and whole-VM cleanup cannot substitute for it.
C must define those bindings using the existing supervisor, and coordinate
durable owner recovery with R/#1676/#1803. The next safely admitted attempt must
recover known interrupted state without manual fence deletion. C must also
publish the exact unit-to-suite mapping with the single D7 schema/API before
consumer handoff. This audit does not establish implementation readiness.

The existing descendant supervisor now accepts explicit internal `forward_fds`
and `close_after_spawn` tuples for a relay experiment. It forwards jobserver
pipes and releases its copy of an exec-start pipe immediately after dispatch,
so Rust can drain captured output before the tool exits. Post-spawn errors stay
inside the original descendant cleanup guard. Storage lease descriptors cannot
be released through this interface. Real subprocess regressions cover large
captured output, exit status, invalid descriptors, storage retention and an
injected post-spawn failure with a detached child. The ordinary Linux CLI suite
runs these cases. No command-line/environment switch enables this interface,
and it establishes no planner credential or artifact isolation.

The interface experiment in `ci-cargo-relay.py` refuses root and uses a private
0700 Unix socket directory, same-UID peer credentials and an immutable nonce-bearing
configuration memfd. Its separate preload library installs the existing
deny-exec filter before main. Hooks translate `execv`/`execve`/`execvp`/`execvpe`
and supported `posix_spawn` calls into bounded requests. The Rust fixture proved
that returning ENOSYS from `posix_spawn` does not establish an `execvp` fallback;
the adapter therefore implements the observed spawn path. It records supported
close/dup/chdir/fchdir actions through their public APIs and forks a proxy that
releases its exec-start pipe only after the delegated tool's launch handshake.
Other actions and non-default process-group/scheduling/credential attributes
refuse. There is no unchecked native execution fallback.

Prototype wire format (little-endian, not C's passing-proof schema): the sealed
160-byte configuration contains `JARVCFG2`, UID/PID/timeout, a 32-byte nonce and
a 108-byte socket path. A 300-byte `JARVREQ2` frame carries the nonce and at most
64 original descriptor numbers; SCM_RIGHTS carries a sealed argument memfd,
stdio and inheritable FIFO descriptors. The `JARVARG2` memfd's 36-byte header contains flags,
argument/environment counts and the original ignored-signal/blocked-mask bits,
followed by length-prefixed file/cwd/search-path/argv/env
bytes, bounded to 8 MiB, 4096 entries and 1 MiB per string. Unknown inheritable
regular/socket descriptors refuse. `JARVRSP2` start/finish/error/signaled frames are
16 bytes. Each tool runs through the original supervisor. The endpoint bounds
concurrency to four, requests to 4096 and each declared tool deadline to at most
60 seconds; uncertain cleanup retains a failing state and never publishes a
passing receipt. Tool environment bytes are applied only by the unprivileged
exec bootstrap, after removing this experiment's preload/control selector.

The original supervisor supplies a pidfd for its actual child through a private
control socket, before the exec-start acknowledgement can complete. That socket
is never inherited by the tool. Asynchronous `JARVSIG2` frames route signal
numbers to that pidfd, with a 4096-message bound and strict descriptor refusal.
The tool receives its requested initial mask and ignored dispositions. The proxy
catches signals even when initially blocked or ignored, so a tool can later
install a handler or change its own mask. Synchronous proxy faults fail rather
than masquerading as tool signals. The supervisor's internal raw-status option
returns the actual wait status only after descendant cleanup; ordinary provider
callers retain their shell-style codes. A private status carrier checked against
the supervisor exit distinguishes signal termination from ordinary exit 143.
The native proxy reproduces a terminating signal after the original cleanup.
Disconnect/timeout cancellation sends TERM once and waits, avoiding a second
signal racing the supervisor's committed status during interpreter teardown.

This is not the accepted planner execution protocol. In particular, queued
signal-info payloads, complete job-control/process-group behavior and other
semantics beyond the tested cases, complete ABI/loader
coverage, separate credentials, immutable per-unit inputs/output publication,
graph-based invalidation and final suite-proof binding remain open. The endpoint
reports metadata isolation and shipping authority as false. No production
launcher, merge consumer or final C schema uses it.

Disposable boundary tests may exercise a specified gate under the existing VM
admission before production activation. They must never fall back to unchecked
privileged execution or create a reusable CI/merge proof. Unsupported capability
observations remain observations; they cannot authorize those tests' privileged
transitions. The credential/loader protocol and fail-closed gate must be resolved
and verified before any experimental privileged Cargo invocation.

Testable build order: query the real guest's kernel ABI and Cargo loader without
granting execution authority; prove denial of direct/bypassed privileged exec,
listener loss, loader/environment substitution and credential recovery; then
prove metadata/artifact isolation against real build scripts/macros/tests. Run
the existing generated-input `OLD`→`NEW`, compile-error/assertion, mutation/revert,
feature/configuration and exact compiler-call reuse cases through the actual VM.
Only then finalize and freshly audit the C mechanism/schema before consumer
handoff. No step here is a passing CI or autonomous-merge claim.

### Host provisioning: KVM access that survives logout (operator step)

On desktop hosts `/dev/kvm` is usually `root:kvm 0660` plus a logind `uaccess`
ACL for the user at the active seat. That ACL is removed when the seat session
ends, after which qemu cannot open `/dev/kvm` and every VM build is denied. Doctor
detects this state (`getfacl /dev/kvm` shows `user:<you>:rw-` while `id -nG` does
not list `kvm`). Grant access through group membership instead. This needs sudo:

```sh
sudo usermod -aG kvm "$USER"
sudo reboot
```

Group membership applies only to processes started after a new login. A user
manager that survives (any remaining SSH, tmux, remote-desktop or other session)
keeps the old groups, and so does every service it restarts, so reboot, or at
minimum end every session of the user, including SSH, tmux and remote desktop,
before starting the daemon again. Then verify:

```sh
id -nG | tr ' ' '\n' | grep -x kvm
augmentagent doctor   # build_vm: ok, not the ACL warning
```

**Not recommended: `loginctl enable-linger`.** Lingering is not needed for this
step (group membership already removes the dependency on the seat ACL). Side
effects: every enabled user unit (the daemon, the updater, auto-PR and wiki-sync
timers, remote-desktop units) starts at boot with no one logged in, spending the
Claude subscription quota unattended while gnome-keyring stays locked, so secrets
read from the keyring fail.

Record the change in the private provisioning record.

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
available before loading 9p. The daemon user needs read-write access to `/dev/kvm`
through the `kvm` group, not only a login-seat ACL (see above).
For Code Mode enablement, pinned guest pip provisioning, limits, CLI QA and
rollback, see [CODE-MODE-COMPUTE.md](CODE-MODE-COMPUTE.md).

The Code Mode compute profile additionally requires the guest's `virtio_console`
driver, either built into the kernel or included in `modules` with its matching
dependencies. Compute transfers bounded logs and requested artifacts through a
root-owned virtio serial port connected to the host's bounded subprocess pipe.
The unprivileged workload cannot open this port, and receives no writable host
share for exports. Results are accepted only after guest shutdown and verified
descendant cleanup. The legacy build profile keeps its existing console transport.
A runtime without the required driver cannot execute compute; it never falls back
to host Python. A UART console is not a substitute for this result channel: full
8 MiB logs must transfer within the configured execution deadline.

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
export JARVIS_TEST_BUILD_SCRATCH=/mnt/build/codex-vm   # an existing 0700 directory
python3 scripts/tests/codex_build_vm_test.py
python3 scripts/tests/codex_tool_bridge_test.py
./scripts/cargo-managed.sh test -p augmentagent-channel-core live_codex_builds_and_tests_with_the_vm_bridge --lib -- --ignored
```

The last command needs working Codex authentication and uses the daemon's default
configuration selection (or its explicit override). It verifies actual broker
execution, a passing Cargo socket test and the tool audit.

Installing runtime files does not deploy or restart the daemon. Retain the previous
binary and runtime configuration for rollback, including private provider cooldown
and handoff state. Restore the prior binary/configuration and restart through the
normal service procedure. Do not delete handoff state to force a retry; the
daemon's own sweep removes only finished, expired journals (see
[journal retention](CODEX-FALLBACK.md#handoff-journal-retention)). Before
rolling back to a version that cannot read the new handoff journals, pause task
intake and scheduled mutation workflows. Reconcile in-flight/uncertain operations
using the current recovery tooling before resuming them; an older binary cannot
safely infer that an unrecognized receipt means an operation did not occur.
Verify saved binary hashes and configuration permissions before replacement,
then verify the running executable and service health after restart.

Build artifacts and writable package caches are private to a bridge session and
live under the scratch root (see above).
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

Cargo uses the session's private writable Cargo home in the build-cache image,
seeded in the guest once per session with the provisioned registry archives, index
and Git cache. It preserves canonical crates.io
identities and can reuse newly downloaded crates on a subsequent offline build.
Those copies never reconcile into source or replace the operator's cache. Within
a session, extracted sources, Git checkouts and build outputs persist, including any
changes a guest build made to them. Every new session starts again from the
provisioned operator cache. Explicit `--offline` and `--frozen` flags retain their offline behavior.
The executable toolchain directory must be owned by
root or the daemon user and must not be group/world writable; provision a private
copy when the ordinary Rust installation uses shared permissions.

Verification now includes uncached npm and Cargo installs, an offline Cargo
rebuild, denied direct networking and HTTP mutations, inaccessible signing keys,
canonical lockfiles and a fresh npm-ci session. A clean worktree of this project
passed installation, build and all 26 Node tests through the gateway, including
compilation of its native SQLite addon.
