# Autoship resource headroom recovery

Native-state capacity and run memory admission are separate checks. A generic
`workflow-resource:capacity-exhausted` can mean `ancestor_headroom`: a shared
runtime slice cannot fit the declared request alongside its charged usage.
Host `MemAvailable` can be sufficient while this ancestor limit refuses.

Run accounting stays conservative. It discounts `inactive_file`, but continues
to charge active file cache and anonymous/kernel memory. Live `run-*` groups
reserve their full ceilings until verified cleanup removes them. Neither
`Committed_AS` nor native-runtime owner row counts determine this headroom.

When a finite ancestor refuses, explicit resource preparation and launch-time
`reserve()` can ask the kernel to reclaim file cache at that limiting ancestor.
They never change memory limits, remove cgroups, delete sessions, or release run
reservations. Anonymous memory is excluded with `swappiness=0`.

The request is made only when active file pages, conservatively reduced by
`file_dirty` and `file_writeback`, cover the charged shortfall. Its size includes
already-discounted inactive file pages plus the shortfall: reclaiming only the
inactive pages need not reduce charged usage. Requests are capped at **4 GiB** without an immediate reclaim loop. If that
cannot clear enough cache, admission stays refused; a later attempt after the
cooldown can reduce remaining cache further. The kernel can reclaim more
or less than requested; this is a request bound, not a guarantee about the
kernel's exact result.

A private, locked file in `/run/user/<uid>/augmentagent-ancestor-reclaim-<hash>`
limits attempts to **once per 600 seconds per ancestor** across worker, daemon,
and fresh helper processes. The hash identifies the boot and cgroup inode.
Failed/partial requests consume the interval too. Unsafe/unavailable rate state
skips reclaim. Rate state is not an admission capability or a resource lease.

After preparation, the helper observes headroom once again; admission still
requires the ordinary counters to pass. Partial reclaim (`EAGAIN`), unchanged
counters, or rate limiting cannot authorize a run. Launch-time admission retains
its existing bounded wait for transient capacity. Preparation retains the Rust
caller's five-second helper timeout and never performs that admission wait.

`run-resource-scope.py --inspect <policy-json>` remains strictly read-only.
`--prepare <policy-json>` explicitly permits the bounded recovery and is used by
runtime resource readiness, so autoship can recover before selecting an issue.
Both are observations, not proof that a later run will be admitted. Launch-time
admission rechecks under its own lock.

Preparation logs `resource ancestor cache preparation` with numeric target inode,
request, limit, before/after memory counters, and a bounded status/errno. The
launch supervisor emits the same numeric reclaim event on stderr. No policy
contents, credential strings, workflow IDs, or cgroup paths are logged.

The behavior is covered by `AncestorCacheRecoveryTests` in
`scripts/tests/run_resource_scope_test.py`, including cross-process rate limiting,
non-reclaimable pressure, bounds, and no-progress/partial reclaim. The tests use
real temporary counter files and simulate only the kernel reclaim write; they
run on the Mac without changing host cgroups. Actual Linux delegated-cgroup and
subreaper tests remain Linux-only.
