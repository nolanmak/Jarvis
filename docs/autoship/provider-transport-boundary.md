# Remaining provider transport boundary (#1453)

Tool commands use the confined command bridge, including its no-network policy.
Native provider processes use `provider_transport` in
`scripts/autoship-scope-guard.py`: their writes and process interference are
restricted, but their filesystem reads and network destinations are not confined
by that guard. Do not describe the tool-level probes as proof of a native
process hostname or credential-read allowlist. This acceptance item remains open.

Landlock's TCP rules control ports, not hostnames. Allowing port 443 does not
enforce a provider-host allowlist. See the
[kernel's network access documentation](https://docs.kernel.org/userspace-api/landlock.html).
Likewise, inspecting a target's `connect` arguments and then using seccomp user
notification's `CONTINUE` flag is not a safe replacement: another target thread
can change those arguments. The
[Linux user-notification documentation](https://www.man7.org/linux/man-pages/man2/seccomp_unotify.2.html)
explicitly warns about that race.

Read-only capability probes on the deployment host on 2026-10-26 found:

- `unshare --user --map-root-user --net true`: UID-map operation denied.
- `bwrap --unshare-user --uid 0 --gid 0 --unshare-net --ro-bind / / -- /usr/bin/true`:
  loopback setup denied.
- Noninteractive privileged execution is unavailable (`sudo -n /usr/bin/true`
  requires a password). No privileged or host network configuration was changed.

A subsequent real guest probe demonstrated an available unprivileged route: the
existing QEMU runtime can launch native Codex and Claude binaries in a guest with
no network interface beyond loopback. Synthetic credential tests denied
undeclared host files; direct IPv4, IPv6 and DNS attempts failed; the existing
controller gateway admitted an allowlisted request and rejected an unlisted
request. See [the implementation plan](native-transport-provisioning-plan.md) and
`scripts/autoship-native-vm-boundary-probe.py`. This proves feasibility, not
authenticated provider inference or streaming.

A complete implementation needs this network-isolated launcher wired to a
controller-owned provider gateway, with explicit provider credential/read grants. Proxy environment variables alone
are not enforcement because a subprocess can bypass them. Provisioning must be
reviewable, scoped to these workers, reversible, and preserve ordinary daemon
updates; it must not silently turn off isolation on an unsupported host.

Exit evidence must include real subprocess attempts to reach an unlisted host
and read unrelated credentials, recorded denial/latching, legitimate provider
authentication and inference, and the same checks for every admitted fallback.
No provider-process host-confinement acceptance or #1453 closure is asserted by
the current tool-bridge and filesystem-write probes.
