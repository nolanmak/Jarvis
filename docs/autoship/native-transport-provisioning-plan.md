# Native provider confinement: implementation and provisioning plan

Status: #1453 remains open. This plan does not authorize a production privilege,
network, daemon-unit, or provider-authentication change. Ordinary interactive
calls and daemon updates must retain their existing behavior.

## Verified current boundary

The native write guard and restricted tool bridge do not confine native-process
reads or network destinations. A disposable same-UID subprocess under the current
`provider_transport` v2 guard successfully read a synthetic protected credential
and connected to a pathname UNIX socket in protected storage. No real secret or
production socket was touched. This host exposes Landlock ABI 8 and systemd 255.
Codex/Qwen/GLM use the Codex native entry point; Claude has its own entry point.
Text-only native reviewers also need the common required-workflow boundary.

Version 3 of `autoship-scope-guard.py` adds explicit read/execute grants and denies
new AF_UNIX sockets. It retains anonymous socketpair for internal IPC and denies
unlisted content reads in descendants. Its subprocess tests verify declared
synthetic auth succeeds, unrelated auth and symlink traversal fail, host sockets
fail, and missing/malformed grants prevent dispatch. It is groundwork, not yet a
production native-launch integration or provider-authentication acceptance proof.
Existing v1/v2 callers remain honestly write-only; no automatic policy downgrade
is allowed when the common launcher is installed for required workflows.

Filesystem-content confinement is not metadata confidentiality. File descriptors
opened before confinement retain authority: the launcher must close unrelated
FDs and pass only declared stdio/control channels. Landlock also does not replace
host isolation for pathname UNIX sockets on ABI 8. The kernel adds that control
in ABI 9; v3 uses syscall argument filtering instead. See the
[kernel Landlock documentation](https://docs.kernel.org/userspace-api/landlock.html).

## Ordered implementation

1. Introduce a controller-owned native launch profile shared by both native
   entry points, enrolled by the protected workflow policy, including text-only
   review and every fallback. Missing/invalid required profile must fail before
   model admission and provider dispatch. Profiles bind provider identity,
   resolved executable/digest, read roots/files, writable runtime, credential
   grant IDs, environment names, and network policy version; receipts contain no
   credential values. Keep the resource ready/continue handshake and accounting.
2. Give each actual native conversation an isolated runtime. Copy only its own
   provider authentication bundle through a controller-only path; never grant
   all of HOME, CODEX_HOME, Claude config, workflow state, or unrelated histories.
   Fresh independent reviewers receive new runtimes. Builder continuation reuses
   its own persisted runtime and actual native session ID. Do not substitute an
   audit UUID or destroy resume state by creating a new home on every call.
   OAuth refresh needs a serialized controller-owned writeback protocol, bound to
   the original auth generation, without copying native settings/hooks/history.
3. Preserve the existing MCP protocol through an outside-sandbox tool broker.
   Today native CLIs spawn the stdio bridge, so that descendant needs the same
   source/policy reads the native process would inherit. Merely granting the
   bridge's policy/source directories to the native parent defeats tighter read
   boundaries. Launch the policy-owning broker as a controller sibling and use a
   thin stdio relay with a per-run authenticated, bounded protocol. All requests
   remain checked against the immutable workflow tool policy. Prefer inherited
   channels where native CLI FD handling supports them; otherwise use a tightly
   scoped local endpoint that grants no general shell or control-plane access.
   No original credential-bearing policy file is readable by the native process.
4. Install v3 read/execute grants for exact provider runtime, resolved binaries
   and necessary libraries/system trust data. Enumerate them from a trusted
   deployment profile, never task/model text. Include no broad `/`, `/home`,
   `/proc`, `/run`, or original provider-config grant. Source reads and Git
   operations flow through the broker. Deny new AF_UNIX sockets; isolate any
   legitimate native IPC separately. A direct caller cannot set an env flag to
   disable a required profile.
5. Provision the network boundary below, then execute every acceptance case
   before declaring the native transport isolated or enabling required dispatch.

## Necessary privileged prerequisite

This host denies unprivileged network namespace creation and noninteractive
privileged execution. A system administrator must provision a narrowly scoped
root-owned launcher/broker capable of creating or entering a worker network and
mount namespace. Giving the daemon unrestricted sudo, CAP_SYS_ADMIN, or generic
StartTransientUnit permission is not acceptable. A system service can establish
`PrivateNetwork`/`NetworkNamespacePath`; a user service on this host cannot bypass
the missing user-namespace capability. See
[systemd execution namespace controls](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html).

The launch broker must authenticate the controller UID, accept only a fixed
versioned launch schema and registered native executable, reject arbitrary unit
properties/paths/commands, drop privilege before provider execution, and return
namespace/cgroup identities before dispatch. Workers remain beneath the existing
aggregate CPU/memory/task ceiling; a sibling system unit must not escape those
limits. The ordinary daemon/updater must not enter the worker network namespace.

Worker namespaces have no general host route, DNS path, inherited network
socket, or host UNIX-socket mount. Their sole external path is a controller-owned
allowlisting transport gateway. Proxy environment variables configure native
clients but are not enforcement. A generic CONNECT tunnel plus allowed port 443
is insufficient for a strict HTTP-host claim: TLS SNI/HTTP authority mismatch,
CDN fronting, redirects, alternate ports, IP literals and resolver rebinding need
explicit handling. Use a trusted HTTP gateway that validates authority and
upstream TLS and permits only configured provider endpoints; if transparent TLS
interception is necessary, provision a worker-only trust root and first prove
native clients support it. The gateway handles credentials as sensitive data and
must not log headers/bodies. If native authentication requires TLS pinning that
precludes this design, use a supported provider-specific gateway transport rather
than silently weakening the guarantee. Endpoint lists must be derived from
observed legitimate authentication/inference and reviewed, not guessed here.

## Binary acceptance and reversible rollout

- Every admitted provider and fallback completes authentication, actual inference,
  fresh independent review and builder session continuation with its own grants.
- Real subprocesses cannot read unrelated credential/audit/approval/budget files,
  follow a symlink to them, read another process's environment/memory/FDs, connect
  host pathname/abstract UNIX sockets, or inherit undeclared open descriptors.
- Without proxy settings, with forged settings, direct IP/DNS/IPv6/UDP attempts,
  an unlisted hostname, redirects or mismatched HTTP authority, external access
  fails. A listed provider endpoint still succeeds. Capture denial receipts
  without secret contents and latch the workflow dependency before publication.
- Missing profile, missing namespace/gateway, expired auth, denied provider host,
  resource exhaustion, cancellation and fallback never retry unrestricted.
  Infrastructure dependencies do not consume code-convergence attempts.
- Kernel/service admission identities and policy digests are protected receipts.
  Lost admission receipt prevents dispatch. Cancellation reaps descendants and
  closes broker channels before releasing the workflow lease.
- Stage artifacts and validation in an isolated test service. Take deployment
  snapshots only with `augmentagent deploy snapshot`. Install the narrow broker
  and required profiles, run synthetic plus live native acceptance, then enable
  autoship dispatch. Rollback pauses affected autoship admission and reverts the
  broker/profile version; it must not silently launch required workers unconfined.
