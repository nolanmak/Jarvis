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

## Runtime requirements shared by the launch designs

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
4. For a host-process design, install v3 read/execute grants for exact provider
   runtime, resolved binaries and necessary libraries/system trust data. The
   preferred VM design instead exposes only declared guest files and devices. Enumerate them from a trusted
   deployment profile, never task/model text. Include no broad `/`, `/home`,
   `/proc`, `/run`, or original provider-config grant. Source reads and Git
   operations flow through the broker. In the host-process design deny new
   AF_UNIX sockets; in the VM design never export host socket paths. Isolate any
   legitimate native IPC separately. A direct caller cannot set an env flag to
   disable a required profile.
5. Implement the preferred no-NIC VM boundary below, then execute every acceptance case
   before declaring the native transport isolated or enabling required dispatch.

## Preferred route: existing unprivileged KVM boundary

**Correction to the initial plan:** privileged host network namespaces are not a
necessary prerequisite on this deployment. The existing owner-provisioned QEMU
runtime and accessible `/dev/kvm` already support a separate guest filesystem and
network stack without changing host namespaces. This route is preferred over
provisioning a privileged launch broker.

A real 512 MiB guest probe passed on 2026-10-26 using the production
`codex-build-vm.py` runner. The guest had only loopback. Direct external IPv4,
IPv6, host-alias TCP, and virtual/external UDP DNS attempts failed. An unmounted
synthetic host credential was inaccessible; the explicitly supplied synthetic
credential remained readable. The existing guest-local HTTPS gateway fetched
76 bytes from its declared registry endpoint and rejected an unlisted hostname
and traversal route. Native Codex 0.164.0 and Claude 2.1.310 binaries both executed
`--version` inside the guest. No actual credentials or model calls were involved.

Reproduce after the central build queue is idle:

```sh
systemd-run --user --wait --pipe --collect \
  --unit=autoship-native-vm-feasibility \
  --property=MemoryMax=1G --property=TasksMax=64 --property=CPUQuota=100% \
  /usr/bin/python3 scripts/autoship-native-vm-boundary-probe.py \
  --execute --native-versions
```

This proves the available isolation substrate and executable compatibility, not
provider authentication, model inference, streaming, session continuation, or a
new provider gateway. The probe has a short deadline; productive native sessions
must not inherit the build runner's 900-second maximum.

### Implementation using the proven substrate

1. Add a dedicated native VM launcher using the trusted `Runtime.load`, initrd,
   QEMU sandbox and lifecycle helpers. Keep `-nic none`, no TAP/bridge, no host
   home, and no host process/filesystem namespace. Export only a clean declared
   candidate snapshot, pinned executable/runtime, and that actual conversation's
   isolated provider auth/runtime. Preserve original provider identity and actual
   session IDs. Source changes return through guarded diff application; never
   export a host checkout containing Git credential config or controller state.
2. Reuse the existing guest-local TLS adapter/mailbox pattern, or the existing
   virtio-serial framing from code-mode compute, for a dedicated provider gateway.
   The gateway must support bounded streaming request/response frames and the
   provider's required HTTP/WebSocket semantics. Validate every upstream
   authority, route, redirect and header on the **host**. Enforce upstream TLS and
   reject IP literals, alternate ports, private-address resolution, DNS rebinding
   and disallowed redirects. Protect authentication values from logs. Guest-local
   trust provisioning must be proven against actual native clients; unsupported
   certificate pinning is a compatibility failure, never a reason to add a NIC.
3. Keep the policy-owning tool broker outside the native worker's authority.
   Guest root/controller helpers may serve the unprivileged provider over stdio
   or a declared local guest channel. A host-side framed broker exposes only the
   workflow's existing tool API, not arbitrary host command/path access. It must
   not trust guest claims about successful review, budget, audit or admission.
4. Wrap QEMU, gateway and broker descendants with the existing required-workflow
   resource/admission and cancellation controls. VM memory plus overhead must fit
   the declared per-run cap; all runs remain under aggregate service limits. No
   provider admission charge before required VM/resource/gateway readiness, no
   duplicate charge per streamed network frame, no whole productive-run timeout.
5. Fail closed for every required native provider and fallback when KVM, runtime,
   pinned executable, explicit auth/runtime or gateway policy is unavailable.
   Unrelated interactive calls and ordinary daemon updates retain their current
   launch policy. Native fullflow and cancellation acceptance must run before
   changing production dispatch.

QEMU's user-mode `restrict=on` networking is another unprivileged option: its
[primary documentation](https://www.qemu.org/docs/master/system/invocation.html)
says ordinary host/outside routing is disabled while explicit forwarding remains
available. It is **not used by this probe**. The staged kernel currently includes
9p support but no staged NIC driver; more importantly, no-NIC transport avoids
having to establish SLIRP DNS/IPv6/forwarding exclusions at all. Do not replace the
proven no-NIC posture with `restrict=on` on the strength of documentation alone.

### Fallback environment prerequisite

If a deployment lacks accessible KVM and cannot provision this user-owned runtime,
a separately reviewed privileged system-service namespace launcher remains an
alternative. The current host's unprivileged namespace failures still matter for
that alternative, but do not block the VM route. Never grant unrestricted sudo,
CAP_SYS_ADMIN or generic transient-unit authority to the application, and never
silently revert required workflows to unrestricted host processes.

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
  snapshots only with `augmentagent deploy snapshot`. Install the native VM gateway/broker
  and required profiles, run synthetic plus live native acceptance, then enable
  autoship dispatch. Rollback pauses affected autoship admission and reverts the
  broker/profile version; it must not silently launch required workers unconfined.
