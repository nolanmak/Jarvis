# Host capacity contract: C0 working decisions

Part of [#1918](https://github.com/nolanmak/Jarvis/issues/1918), under #1917
and #1703. This is an incomplete contract under review, not an implementation
handoff or an assertion about deployed behavior. The
[feature plan](host-capacity-sizing-proposal.md) remains the scope authority.
Decisions below resolve specific questions; the open-contract checklist prevents
those decisions from being mistaken for completion of C0.

The separately closed [memory component](host-memory-facts-contract.md) permits
implementation of M1-M3 data helpers and the existing supervisor's input decoder
before the rest of C0 closes. References below to blocked C1 mean complete C1
and integration/activation; this bounded exception is not a full contract handoff.

## 1. Memory quantities and planning

M1. All memory values are checked unsigned byte integers. GiB means 2^30 bytes;
MiB means 2^20 bytes. Overflow, an invalid finite limit, missing required data,
or a changed resource identity produces `unknown`, never saturation to an
apparently admissible value. Kernel `max` is an absent enclosing ceiling, not a
missing sample. Host physical memory is always a required finite ceiling.

M2. Define `E` as the minimum of physical RAM and finite enclosing external
`memory.max` limits. Exclude only output scopes whose installer provenance is
verified for this deployment: both the managed service and aggregate runtime
slice. An unrecognized scope, an operator drop-in, or uncertain provenance is
not excluded. This exclusion is only for planning future managed settings.
CPU, storage, swap and current free memory do not increase `E`.

M3. Default protected memory is `H = max(1 GiB, ceil(E / 4))`. Desired managed
aggregate `MemoryMax` is `B = max(0, E - H)`; desired `MemoryHigh` is
`floor(9 * B / 10)` with overflow-safe arithmetic. This leaves one quarter of
the effective machine envelope for the OS and unrelated work, with a 1 GiB
minimum on small machines. It is a conservative initial policy, not a measured
claim about the owner's workload. An explicit reserve may increase H. Lowering
this protected default is not part of automatic sizing.
H protects the total capacity outside the managed envelope. It is not an
additional requirement to keep H bytes unused after current OS/unrelated usage
has already been reflected in an availability/charged-memory observation.

Current free memory does not rewrite these desired settings every tick. It
constrains immediate admission in section 2. `MemoryHigh` is a throttling
threshold; it is not a second hard ceiling subtracted from `MemoryMax`.
Managed service and shared-slice limits are enclosing ceilings on the same
work, not independently spendable pools. Deployment must publish/read back the
complete managed set before claiming it effective.

M4. The daemon is part of B. For admission, reserve at least 512 MiB for the
managed non-run processes, or their greater observed non-reclaimable usage.
Count each run's complete declared memory ceiling, including idle preflight
runs, until existing verified descendant cleanup removes the reservation.
Do not count eight retained disk sessions as eight live memory reservations.

M5. Per-run memory remains the existing producer declaration. Native guest RAM,
an enabled host build guest, and controller overhead are additive. Current
defaults compose `max(4 GiB, native_guest + build_guest + 1 GiB)`; explicit
values must cover the sum. A 1 GiB native guest with a 4 GiB build guest needs
6 GiB; without a build guest the default is 4 GiB. The 512 MiB managed daemon
reserve is outside that per-run controller overhead. Never shrink guest RAM,
overhead or a declared run ceiling merely to fit the host.

### Exact planning fixtures

These fixtures have unlimited external ancestors unless stated, no explicit
override, and verified managed provenance. `daemon` is the admission reservation,
not another subtraction from B. Headroom is calculated from E, so a 2 GiB
external ancestor on a 64 GiB host is handled as a 2 GiB envelope.

| Physical RAM | External cap | E | H | B | MemoryHigh bytes | daemon | 6 GiB builder fits B? |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 2 GiB | none | 2 GiB | 1 GiB | 1 GiB | 966367641 | 512 MiB | no |
| 8 GiB | none | 8 GiB | 2 GiB | 6 GiB | 5798205849 | 512 MiB | no |
| 16 GiB | none | 16 GiB | 4 GiB | 12 GiB | 11596411699 | 512 MiB | yes, one |
| 64 GiB | none | 64 GiB | 16 GiB | 48 GiB | 46385646796 | 512 MiB | yes, seven |
| 64 GiB | 2 GiB | 2 GiB | 1 GiB | 1 GiB | 966367641 | 512 MiB | no |

The last column is a static memory-only bound with no existing runs. It is not
a concurrency grant: live memory, CPU, PIDs, FDs, native state, storage, owner
authorization and fairness still apply. The 8 GiB fixture deliberately refuses
the current full build composition; two guests plus protected service/host
capacity cannot be promised merely because a machine has 8 GiB RAM.

## 2. Immediate memory admission

M6. Reuse the descendant supervisor's existing serialized admission operation.
Its memory accounting must cover the entire managed aggregate shared by daemon
and worker, not two independently admitted service pools. Migration cannot enable
auto mode until that common accounting scope is verified. No second allocator
or process-local snapshot decides concurrent admission.

Let Q be the new run's complete declared memory. For each complete O3 pass i,
let R_i sum existing full run reservations and non-run charges including M4's
daemon minimum; let C_i be the non-reclaimable charge in the same aggregate.
R_i also includes aggregate kernel charges absent from child accounting, as the
current supervisor does. Cover any observed aggregate excess with
`R_i=max(R_i,C_i)`; a child is never counted again as an unrelated sibling.
Compute `S_i=max(0,R_i-C_i)` from that pass's consistent accounting.
If hierarchical accounting is inconsistent or either pass is incomplete,
return unknown and resample rather than selecting the convenient pass.

For admission use `R=max(R_A,R_B)` and `S=max(S_A,S_B)`. A positive charge
pressure predicate may use `C_upper=max(C_A,C_B)`, but must never derive S as
`max(0,R-C_upper)`: maximizing a subtracted charge can hide future reservation
slack. Table fixtures below have identical per-pass accounting unless stated.

Let F_mem=256 MiB be the runtime free-memory margin, retaining the current
supervisor's host margin. F_mem and H answer different questions: H caps total
managed capacity through B, while F_mem leaves immediate spare capacity after
current usage and unpaid reservation slack. This version adds no configurable
F_mem override; the M3 reserve override changes H/B, not F_mem. Do not infer
F_mem from retained disk charges, current free RAM or a desired service limit.

All following predicates must hold on fresh, identity-checked observations
inside the existing admission lock:

1. `R + Q <= min(B_loaded, B_current)`, where B_loaded is the verified,
   currently enforced managed envelope, not an unpublished desired B, and
   B_current is B recomputed from the current verified external envelope.
   Shrink takes effect immediately for new admission; an unpublished increase
   cannot exceed the still-enforced B_loaded.
2. For each actual managed service/slice ceiling on the new run's enforced
   ancestor chain, complete reservations within that scope plus Q fit its
   effective hard limit. A worker run is not additionally charged against a
   separate daemon-only service ceiling. Existing reservations from those
   services meet at their actual shared enclosing aggregate. Unknown membership
   blocks admission; the planner cannot choose a more convenient scope. Include
   every enforced ancestor limit until reload/readback proves it changed.
3. `Q + S + F_mem <= MemAvailable` on the host.
4. For each finite external ancestor, `charged_ancestor + Q + S + F_mem <= limit`.
   Ancestor charges include competing siblings. Subtract its verified reclaimable
   inactive-file cache as in the existing supervisor; unavailable accounting is
   unknown, not an invented zero. For a limit tighter than the plan, recompute
   the smaller envelope before admitting; never reuse a larger H/B pair to
   manufacture a fit.

When integrating these predicates, preserve the existing typed host_headroom/
ancestor_headroom refusal codes and populate the complete operands. Host facts
carry requested_bytes=Q, reservation_slack_bytes=S, margin_bytes=F_mem,
required_bytes=Q+S+F_mem and available_bytes=the selected MemAvailable.
Ancestor facts carry current_bytes=its upper charged usage, limit_bytes=its raw
ceiling, Q/S/F_mem and required_bytes=current_bytes+Q+S+F_mem. Existing
shortfall/reclaim/wait consumers must use required_bytes minus available_bytes
or limit_bytes respectively, with checked arithmetic. Reusing the old ancestor
shortfall expression without F_mem would understate the new guard's deficit.
Unknown operands never produce a fabricated numeric shortfall.

H is already excluded from B_loaded/B_current. Do not subtract it from B again
or substitute it for F_mem in a current-usage check. Both current OS/unrelated use
and any remaining capacity protected by the managed envelope participate in
physical pressure; the quarter is not a second unused-memory floor. S accounts
for idle runs' future use because MemAvailable/ancestor charges already reflect
current usage; adding all R there would count live usage twice. The aggregate,
actual-scope, host and ancestor predicates still all apply, preserving complete
run ceilings and the protected managed cap even when an availability check fits.

This resolves the previous draft's use of H in both planning and runtime-free
predicates. It changes that proposed policy, not the deployed supervisor: the
existing host F_mem stays 256 MiB, and the proposed external-ancestor F_mem guard
is an additional bound. The kernel describes
[MemAvailable](https://docs.kernel.org/filesystems/proc.html#meminfo) as an
estimate for new allocations without swapping; it is not guaranteed physical
isolation from later unrelated allocations.

For an existing fixed deployment, retain its current supervisor checks until
the supported migration installs the complete auto contract. This document does
not authorize a partial admission-policy rollout or a floor override.

### Exact admission fixtures

All values below are GiB; B_loaded=B_current=12, H=4, F_mem=0.25, Q=6.
All actual managed-scope bounds fit unless the aggregate row says otherwise.
In the table rows the external ancestor is the host and supplies no tighter
independent bound.

| R | C | S | MemAvailable | R+Q | Q+S+F_mem | Result |
| --- | --- | --- | --- | --- | --- | --- |
| 0.5 | 0.5 | 0 | 12 | 6.5 | 6.25 | memory admissible |
| 0.5 | 0.5 | 0 | 9 | 6.5 | 6.25 | memory admissible; outside use is already observed |
| 0.5 | 0.5 | 0 | 6 | 6.5 | 6.25 | host pressure, deficit 0.25 GiB |
| 6.5 | 2.5 | 4 | 14 | 12.5 | 10.25 | aggregate deficit 0.5 GiB |
| 4.5 | 1.5 | 3 | 9 | 10.5 | 9.25 | host pressure, deficit 0.25 GiB |

Busy-host counterexample: physical=16, H=4, B=12, R=C=1, Q=6,
MemAvailable=7. Aggregate required=7 and host required=6.25 both fit. The old
draft incorrectly required 10 free despite its managed cap already protecting
the outside share. This is a memory-only fixture, not an overall run grant.
An idle retained native disk session never increases R/C/S in this fixture.

Higher protected reserve remains binding: physical=16, explicit H=8 yields
B_current=B_loaded=8. With R=C=3, Q=6 and MemAvailable=12, host required=6.25
fits but aggregate required=9 exceeds B by 1 GiB. The runtime margin does not
undo an explicit protected-envelope override.

External-ancestor example: E=16, B_loaded=12, H=4, R=C=0.5,
Q=6, MemAvailable=20 on a larger physical host; an external limit of 16
with charged usage 10 requires `10+6+0+0.25=16.25`, so refuse with ancestor
deficit 0.25 GiB. Large host availability cannot override that ancestor.

Inclusive boundary pairs (all other predicates pass):

- Host required=6.25 GiB=6710886400 bytes: availability exactly 6710886400
  passes; 6710886399 refuses with a one-byte deficit.
- Ancestor charged=10 GiB, Q=6 GiB, S=0 and F_mem=0.25 GiB require
  17448304640 bytes: an ancestor limit exactly that value passes; one byte
  less refuses with a one-byte deficit. Recompute current E/H/B for either
  limit; they still fit the other predicates, rather than inheriting a stale
  E=16 from the preceding example.

Swap is never added to RAM. Swap exhaustion alone is reported as pressure, not
used to invent an admission capacity or force-kill sessions. Any new PSI/event
based refusal threshold requires a separately specified contract and fixtures;
this version relies on the hard and available-memory checks above.

## 3. CPU ceiling and scheduling

C1. CPU bandwidth is a rate ceiling, not a promised minimum throughput or an
exclusive reservation. In cgroup v2, `cpu.max` is quota/period; cpuset effective
CPUs restrict placement. Nested ceilings cannot override their ancestors.
See the [kernel controller contract](https://docs.kernel.org/admin-guide/cgroup-v2.html#cpu).
Use the effective CPU affinity inherited by this launch, intersected with the
applicable effective cpuset. Never use the observer terminal's affinity in
place of the worker/supervisor's. Count logical CPUs; do not assume SMT siblings
are separate physical cores or promise a performance multiplier.

C2. Retain CPU rates as exact positive rational numbers until conversion to the
existing integer-percent policy. Let `E_cpu` be the minimum of the usable CPU
set's cardinality and every finite external quota/period ratio. Verified managed
output quotas are excluded only when planning their replacement, as for M2.
Desired managed aggregate quota is `floor(75 * E_cpu)` percent of one CPU.
The remaining quarter is protected from consumption by Jarvis's managed work;
this is a bandwidth ceiling, not a reservation the kernel gives to the OS.
Use a 100000 microsecond period and quota `percent * 1000` microseconds, with
overflow checks. Flooring never raises a measured ceiling.

C3. Values below 1 percent after this conversion cannot be represented by the
current supervisor contract and produce `cpu-below-supported-minimum`, with
the nonzero detected rational rate retained in the observation. They are not
reported as zero detected CPUs. Larger fractional rates such as half a CPU
remain supported. The period/quota pair must be accepted and read back on the
supported host before activation; inability to enforce it is unsupported,
not an excuse to round upward.

C4. The existing run declaration's CPU value (default 200 percent) is a maximum,
not mandatory reserved bandwidth. In auto mode, set the run maximum to the
minimum of that declaration and all currently enforced CPU ceilings on its
actual ancestor chain, rounded down to a supported integer percent. Also cap
it at the current recomputed auto budget. Explicit operator maxima still cap
the result. This clamp must be versioned/visible with desired and effective
values; fixed mode preserves the old declaration/refusal behavior.

Unlike guest memory, reducing this scheduling ceiling does not resize a guest
or remove a memory guarantee. No CPU-based concurrency multiplier is inferred:
admitted runs share their ancestor's quota under the existing fair scheduler.
Use equal run `cpu.weight` (100); do not infer private progress or paid-call
permission from a CPU rate. The existing workflow fairness, bounded calls and
resource cleanup still apply. Instantaneous CPU utilization is not spare quota
to reserve, and a busy sample cannot erase an existing run's accounting.

### Exact CPU fixtures

No operator override; the managed quotas have been successfully installed/read
back; run maximum is the default 200 percent. The affinity and cpuset column
is the final intersection, not the host's total CPU count.

| Effective CPUs in set | External quota/period | E_cpu | Managed percent | Run percent |
| --- | --- | --- | --- | --- |
| 8 | unlimited | 8 | 600 | 200 |
| 8 | 400000/100000 | 4 | 300 | 200 |
| 2 | 400000/100000 | 2 | 150 | 150 |
| 8 | 50000/100000 | 1/2 | 37 | 37 |
| 1 | 1000/100000 | 1/100 | 0 after conversion | refuse; detected 1/100 |

Reload-boundary fixture: desired managed percent=300, currently enforced
managed percent=100, declared run maximum=200. The run ceiling stays at 100
until actual reload/readback; a desired increase never grants 200 early.
If a new external constraint lowers the current budget to 37, the next run
is capped at 37 even if the old managed aggregate is still 300.

## 4. Memory/CPU observation and freshness

O1. Discovery is a read-only producer. Its private version-1 result contains:

- `schema_version`, `status` (`known`, `unknown`, `unsupported`), a stable
  `reason_code`, sample start/end CLOCK_BOOTTIME nanoseconds and kernel boot ID.
- The original worker/supervisor PID plus `/proc/PID/stat` start time, held
  process identity, UID, cgroup namespace identity and cgroup-v2 mount identity.
  A numeric PID without its start time and held live identity is insufficient.
- Policy revision/digest, demand revision/digest and managed-provenance revision.
  Auto/fixed mode and desired/published/effectively-loaded revisions are distinct.
- Actual cgroup membership and the complete enclosing chain, identified by
  held directory descriptors, device/inode, mount identity and namespace. Each
  node has raw limits, observed charges/cache and a separately justified
  managed/external classification. Paths alone are not object identities.
- Physical RAM, MemAvailable, CPU affinity/effective cpuset, exact quota/period
  rates and per-field errors. Parsed `max` is typed `unlimited`; missing is typed
  `unknown`. Unknown fields do not contain a fabricated numeric zero.
- Derived E/H/B, runtime free-memory margin F_mem and CPU values, existing
  reservations, proposed complete run demand, every evaluated predicate and all limiting deficits. An observation
  has no launch-grant field. Public/private projection is specified separately.

O2. The observing context must be the actual worker/supervisor selected by the
existing worker receipt, or an original child whose inherited membership and
affinity are verified against it. A CLI observer cannot substitute its own
environment, cgroup, daemon PID or policy when that receipt is stale/missing.
Planning an installer output may separately inspect the verified external chain;
record that as a plan, never as the current worker's effective run observation.

O3. A coherent-enough sample uses this ordered protocol:

1. Open and hold the process/scope identities. Read boot, process start time,
   membership/namespace, policy/demand/provenance revisions, affinity/cpuset,
   enclosing object identities and limits (pass A).
2. Read counters, host availability and reservations. Discovery uses only
   nonmutating observation APIs: it never invokes storage `Admission::lock`,
   `claims()`, reconciles CI holds, enables controllers, creates a cgroup, writes
   policy or publishes a reservation.
3. Read the same identity/configuration/limit set again (pass B), plus a second
   counter/availability sample. A/B identities, revisions, membership, affinity,
   cpuset and raw ceilings must agree. Derive non-reclaimable charges and the
   full R_i/S_i reservation accounting within each complete pass, clamping
   charge subtraction at zero. Use upper per-pass charge bounds for additive
   pressure checks and lower availability for free-space checks. Memory slack
   is a separately selected upper bound `max(S_A,S_B)` as in M6, never derived
   by subtracting an upper charge bound. Do not combine a child/non-run sample
   from one pass with parent accounting from the other to synthesize R_i/S_i.
   Incomplete/inconsistent accounting makes that attempt unknown.
4. Sample duration must be at most 250000000 ns. A completed sample is usable
   only while `now_boottime - start_boottime <= 500000000 ns`, on the same boot,
   with nondecreasing clock values. At most two full collection attempts are
   allowed per observation request. A second identity/configuration change or
   elapsed bound returns `observation-unstable`; unreadable inputs return
   `observation-unavailable`. Never loop indefinitely to obtain a convenient
   sample. The caller's bounded helper/watchdog also bounds a nonresponding
   collector; a response arriving after expiry is not accepted.

This protocol detects replacement and bounds observation age; it is not an
atomic kernel snapshot or a claim that unrelated applications cannot allocate
after the read. Ancestor limits and per-run kernel enforcement remain necessary.
Admission must not reuse a “known” result as an authorization token.

O4. The existing resource admission owner holds its common allocation lock,
collects/revalidates this evidence in that context, and checks the current
reservation set and complete demand before publishing the new reservation.
If the sample expires or any bound identity/revision changes before publication,
discard it and use the bounded resampling outcome above. Discovery never takes
that mutating authority merely to offer a status preview.

Before paid model release, the existing original process-group/resource
boundary verifies actual new-run membership, applied limits and unchanged
bound policy/demand/managed revisions. A replaced scope, missing readback,
reduced ancestor or invalidated reservation remains closed with a capacity
outcome; any created descendants follow checked cleanup/uncertain-hold rules.
An inspection record or serialized resource facts cannot substitute for the
original execution authority/handles. Live shrink after admission is still
possible; existing kernel enforcement and truthful failure recovery handle it.

### Exact observation fixtures

- Collection 100 ms, used at age 400 ms: fresh if every identity/revision still
  matches. The same result used at age 500 ms is fresh; 500 ms plus 1 ns is stale.
- A 250 ms collection is within bounds; 250 ms plus 1 ns is not. A late first
  attempt may retry once; two unstable/late attempts return unknown.
- PID is reused with another start time, cgroup is replaced at the same path,
  worker changes namespace/membership, CPU is removed from effective cpuset, or
  policy changes between A/B: discard that attempt, never join the two versions.
- Current memory reads 3/4 GiB, reclaimable cache 1/0.5 GiB, host availability
  10/9 GiB: per-pass charges are 2/3.5 GiB; use upper charge 3.5 GiB for an
  additive pressure predicate and availability 9 GiB. Independently derive
  reservation slack from each pass; that upper charge is not its subtrahend.
- Stable R_A=R_B=6.5 GiB, aggregate C_A=4.5/C_B=2.5 GiB, Q=6 GiB,
  host availability 9 GiB in both passes and a sufficient managed envelope:
  S_A=2/S_B=4 GiB, so host required=10.25 GiB and refuses by 1.25 GiB.
  Subtracting C_upper=4.5 instead would falsely require only 8.25 GiB.
  N2 must expose selected reservation_slack=4 GiB, not recompute it from maxima.
- The status collector's cgroup differs from the selected worker: do not use
  local fallback. Return unknown unless it observes the verified target context.
- The boot ID changes, clock goes backward or required controller accounting
  disappears: unknown; previous-boot/timestamp data cannot grant recovery.

O5. Required fields above define the memory/CPU observation contract only.
Storage mount/volume identity, logical image checks, filesystem/inode/quota
support and durable reservation observations must be completed below before C0
closes. A partial memory/CPU result never claims overall capacity is known.

O6. Section 5 extends the same identity/clock/read protocol with typed per-scope
pids.max/current, host task counts and kernel ceilings, the actual launching
process's soft/hard NPROC and NOFILE, real/effective UID and namespace identity,
same-real-UID task counts, file-table counters, nr_open, inherited descriptor
indices, privilege-enforcement readback and the enabled complete role declarations.
Valid zero ceilings are known exhausted limits; they are not missing data.
Unlimited NPROC is distinct from an unavailable threshold. Same-UID usage is
not a substitute for identifying the launcher's own enforced process limit.
Limits/declarations/identities must agree across passes; use greater task/file
usage for additive pressure and smaller availability. P3 reservation slack is
separately maximized from consistent per-pass accounting; a maximum observed
task count cannot be subtracted to minimize that slack. Newly applied child limits/privilege state are
verified at the original pre-release boundary rather than trusted from a saved
JSON report. No overall known result is claimed before storage evidence exists.

O7. Quota absence is an observed state, not an assumption from `df`, mount-option
strings, `statvfs` free bytes or a failed quota read. The storage collector queries
`quotactl_fd(QCMD(Q_GETFMT, type))` for user, group and project quota types on the
held target filesystem descriptor in both O3 passes. `Q_GETFMT` is read-only and
requires no special quota privilege; do not use `Q_SYNC`, quota enable/disable,
or a privileged helper as a discovery substitute. The final implementation uses
the platform's UAPI binding, not the experiment's x86-64 syscall constant.

For this query, success means quota accounting is active, even if no applicable
limit has yet been read. `ESRCH` means that type is inactive. Permission failure,
unsupported syscall/filesystem/type, invalid descriptor or another error is
unknown, never inactive. A/B quota states/formats must agree. Inactive for all
three types removes only the quota predicate; physical bytes, inodes, logical
images and durable commitments still need their own checks. An active type
requires complete applicable owner/group/project usage, limits, grace and
identity observations and corresponding admission predicates. Until that active
profile is specified and implemented, it is unknown; it cannot be ignored merely
because this host currently has none. Quota enforcement can change after a
sample: revalidate at admission, preserve kernel enforcement, and report later
EDQUOT as a runtime resource failure subject to checked recovery.

Sources: [quota descriptor API](https://man7.org/linux/man-pages/man2/quotactl.2.html),
[Q_GETFMT](https://man7.org/linux/man-pages/man2/Q_GETFMT.2const.html), and the
kernel's [`check_quotactl_permission`/`quota_getfmt`](https://github.com/torvalds/linux/blob/master/fs/quota/quota.c).
The read-only experiment `probes/storage_quota_observation.py` returned ESRCH for
all 12 queries across the current state/build/store/archive devices; separate
`findmnt` inspection identified ext4. These are live CLI-context samples, not
deployed-worker observations, an admission receipt, or complete filesystem-profile
certification. No quota configuration, accounting row or production file changed.
The recorded input order was repository/state device, `/mnt/build`, `/mnt/store`,
`/mnt/archive`. The probe emits each held descriptor's device major/minor for
correlation; it does not assert that positional sample indices are independent
filesystems. Reproduce from this checkout with
`python3 docs/autoship/probes/storage_quota_observation.py . /mnt/build /mnt/store /mnt/archive`
and separately inspect each input using `findmnt -T <input> -o FSTYPE,OPTIONS -n`.

Quota fixtures for O7:

- All three queries return ESRCH in both passes on an otherwise verified
  filesystem: quota state is known inactive; do not manufacture unlimited
  physical/image capacity or bypass other predicates.
- A type returns active format 2 in both passes but applicable quota usage/limits
  are missing: overall storage remains unknown, even with abundant `f_bavail`.
- A returns ESRCH and B succeeds: discard and apply O3's bounded resampling,
  never choose the inactive half of that sample.
- EPERM/EACCES, ENOSYS, EINVAL or an unreadable target remains unknown. None
  authorizes setting quota availability to the filesystem's free bytes.

O8. The first storage identity/counter profile is local ext4 with a nonzero
16-byte filesystem UUID returned by descriptor-based `FS_IOC_GETFSUUID`, and
the required descriptor/statx/mount-namespace observations below. This is not
automatic support for every filesystem that happens to implement that ioctl.
Missing/incompatible required interfaces return unsupported or unknown as
appropriate; never guess identity from a pathname, volume label or `df` line.
No block-device read, UUID setter, mount operation or privileged discovery
helper is needed. Final producer allocation/metadata profiles remain separate.

Use these two distinct bindings:

- Persisted `FilesystemIdV1 = {kind:"ext4", uuid_hex:<32 lowercase hex digits>}`.
  This identifies the intended filesystem across ordinary reboot; store it in
  root membership and commitment components rather than persisting `st_dev` or
  a mount ID as a reboot-stable identifier. It is descriptive accounting data,
  not proof of original ownership, source fidelity, execution or release.
- Live attachment: current boot, actual selected worker's held mount-namespace
  device/inode identity, filesystem type/full UUID, device major/minor, unique
  statx mount ID, and each held target directory/file's inode and type. Require
  the `STATX_MNT_ID_UNIQUE` result mask, not just a numeric field defaulting to
  zero. Hold original descriptors throughout sampling and publication; each
  associated canonical path must still resolve to its held object in that
  actual namespace. A held old descriptor alone does not prove the published
  path still selects it. The final root/database/anchor binding is additional.

Within one verified boot/worker namespace, paths with the same ext4 UUID and
same current device contribute to one filesystem balance even when mount IDs
or directory inodes differ. Mount IDs identify attachments, not separate pools
of free bytes. The same UUID appearing on different current devices, or one
device returning contradictory UUID/type, is an identity collision/unstable
observation: do not merge, double-spend, choose one or mint another filesystem
ID to obtain capacity. Across reboot, re-resolve the registered canonical root
and full UUID and verify its original database/anchor binding before attaching
existing rows to the newly observed device/mount IDs. Do not carry previous-boot
availability, mount IDs or live grants forward.

Filesystem UUIDs and anchor contents can be copied. Matching them does not
establish native source provenance or recover original controller/custody
handles; those checks still govern execution, migration and retirement. Known
collisions/missing or contradictory roots remain held. This profile does not
claim to detect every owner-made filesystem clone from metadata alone.

For output roots, resolve and hold the actual intended directory without
symlink/magic-link substitution and revalidate its path attachment. Discovery
does not create missing directories. A planned new child is bound to its exact
held parent plus one validated basename, not an arbitrary nearest existing
ancestor. Its eventual creation/publication must use that original bound parent
and verify the resulting child/device before any covered growth/model release.
Do not let a missing/unmounted output path silently borrow its parent device's
free space. Full bridge/constructor support for these held bindings remains a
C0 API/integration gate; converting them back to an unchecked path is not support.

On held descriptors collect `fstatfs` type/identity and `fstatvfs` counters in
both O3 passes. Counter units are bytes from checked `f_bavail * f_frsize`, and
available inode count from `f_favail`; use the smaller availability of the two
passes. Require positive unchanged `f_frsize`, and coherent nonnegative
`f_bavail <= f_bfree <= f_blocks`, `f_favail <= f_ffree <= f_files` counts.
Overflow, invalid ordering or a changed identity/size/unit makes that attempt
unknown/unstable. Zero byte/inode availability is known exhausted, not unknown.
Do not use `f_bfree` (which may include reserved blocks) as user availability,
multiply by an assumed 4096, or treat `f_frsize` as a demonstrated future
allocation-cluster/metadata growth bound. A read-only/incompatible mount does
not qualify as a writable storage profile from nonzero free counters. Full
write-profile validation, inode floors/demand and active-quota/image predicates
remain required in addition to these observations.

The inspector reads the durable root registry and each native balance in
read-only mode with zero busy wait, validates identity/schema and counts full
pending/held/release-pending rows. It never deletes claims, reconciles CI holds,
creates an anchor, imports missing records or marks a row released. Two-pass
inventory revisions must agree. Actual admission recollects/revalidates this
inventory under S6's original common lock, including current CI uncertainty
holds and complete demand. A busy, missing or contradictory registered database
cannot disappear from the sum. Exact SQL/revision encoding is still a C0 gate.

Identity/counter fixtures:

- Two held directories on one verified filesystem have distinct directory
  inodes but one UUID/device: one pool; sum distinct demand components once.
- Bind aliases have distinct attachment IDs but the same verified UUID/device:
  one pool, with each target's own attachment still checked. Actual bind-mount
  fixture evidence remains required; the disposable probe tests directories only.
- A directory is renamed away and another created at its path: the old held
  descriptor stays internally stable, but path-to-held identity disagrees and
  the observation cannot authorize publication against the replacement.
- Reboot changes device/mount IDs with matching registered filesystem and
  original root/database/anchor: reobserve and rebind accounting only; no saved
  resource snapshot or original execution/cleanup capability is recreated.
- `f_bavail=100`, `f_frsize=4096`, `f_favail=10` yields 409600 bytes/10 inodes.
  A second sample of 90 blocks/9 inodes yields 368640 bytes/9 inodes.
- An archive path resolves onto the system device after mount loss: its
  registered UUID disagrees and capacity remains unknown/held, regardless of
  system-device free bytes. Missing registry/anchor never selects a fresh pool.

Sources: kernel [filesystem UUID UAPI](https://github.com/torvalds/linux/blob/master/include/uapi/linux/fs.h),
[statx](https://man7.org/linux/man-pages/man2/statx.2.html),
[statfs](https://man7.org/linux/man-pages/man2/statfs.2.html) and
[statvfs](https://man7.org/linux/man-pages/man3/statvfs.3.html).
The non-root probe `probes/storage_filesystem_observation.py` read full UUIDs
and held-descriptor counters on all four current devices; it emits no raw UUIDs.
Its three disposable directory replacement/same-filesystem predicates passed.
These are exploratory CLI/kernel facts, not two-pass worker collection,
bind-mount/reboot tests, complete storage admission or root-anchor migration.
The probe also emits pairwise UUID/device equality/collision facts for supplied
inputs. The four current devices compare distinct with no collision. A repeated
repository path compares equal in both fields; that alias experiment is still
not a privileged bind-mount or worker-namespace test.

## 5. Host tasks and descriptor readiness

P1. Host task demand is the existing run declaration's full `pids` value,
default 512, not its current number of processes. The cgroup PID controller
counts kernel tasks/threads, including descendants. Guest processes inside a VM
are not additional host PIDs; QEMU's host threads and host bridge/build processes
are. Preserve the guest's separate limits (currently NPROC 128 and NOFILE 256)
without adding them as host reservations. See
[kernel PID-controller semantics](https://docs.kernel.org/admin-guide/cgroup-v2.html#pid)
and `native-provider-vm-guest.py`'s worker limits.

P2. Reserve at least 64 host tasks for managed non-run processes, or their greater
observed task count. Do not shrink the run's declared task ceiling to make it
fit. Retain each run's full task reservation until checked descendant cleanup
removes its resource scope, including an idle closed preflight scope.

For desired managed `TasksMax`, calculate a memory-based upper bound on the
tasks that the configured role set could admit. Let D be M4's non-run memory
reservation, A=max(0,B-D), and K=max(role_pids / role_memory_bytes) over every
enabled role's complete validated declarations. K is an exact rational, not an
integer truncation. Desired tasks are `D_tasks + floor(A * K)`, capped by each
finite external `pids.max` less 64 protected tasks, and by
`min(kernel.threads-max, kernel.pid_max - 1) - 64`. Clamp a negative usable
ceiling to zero. An explicit operator task ceiling may only lower this result.
`D_tasks=max(64, observed_nonrun_tasks)`. All arithmetic is checked.

This is a managed ceiling, not a promise to start floor(A/min_memory) runs.
Changing the enabled role declarations changes the plan revision. Admission
uses the currently enforced TasksMax until verified reload and cannot spend a
desired increase. An empty/invalid role set or unavailable required kernel/task
evidence is unknown; do not fall back to the old 2048 just because it is familiar.

The role universe is the configured routes that can launch under this managed
aggregate, not every helper executable and not only the selected next workflow:

| Role identifier | Complete declaration producer | Unmodified defaults |
| --- | --- | --- |
| `host-text` | `run_resources::declared_policy_for(Text)` when an existing allowed non-native text route is enabled | 1 GiB, 512 host tasks |
| `host-builder` | `declared_policy_for(Builder)` for an existing allowed host provider with autoship scope | 4 GiB, 512 host tasks |
| `native-without-build` | `native_vm::Launch::prepare` via `composed_native_policy(policy.memory_mb, 0)` | 4 GiB, 512 host tasks with a 1 GiB native guest |
| `native-with-build` | Same composition with the actual enabled bridge/build guest demand | 6 GiB, 512 host tasks for 1 GiB native + 4 GiB build + 1 GiB overhead |

Use configured validated overrides and the actual enabled provider/bridge
composition, not those defaults, in K. An ordinary 1 GiB host-text declaration
cannot stand in for a native text turn's complete composed ceiling. Disabled
routes do not contribute; staged but unauthorized routes are not made enabled
by this table. Helpers inside a run are already part of its declaration and
do not add another role. A route outside this inventory requires an explicit
validated declaration before auto activation, never an inferred guest-only size.
This resource inventory does not authorize new host execution or relax isolation.

P3. Within the common supervisor admission operation, for the actual scope
chain of the new run, compute per complete pass i: R_tasks_i is existing full
run declarations plus max(64, non-run task use), covering any greater observed
scope charge; C_tasks_i is that scope's observed tasks; and
S_tasks_i=max(0,R_tasks_i-C_tasks_i). Use upper R_tasks=max(R_tasks_A,R_tasks_B)
and S_tasks=max(S_tasks_A,S_tasks_B), retaining scope/UID membership for each
applicable slack. Do not compute slack by subtracting the greatest observed
task count from a separately combined reservation count. Additive external/
host/UID pressure counts use their upper per-pass observations. Admit only if:

- R_tasks + requested_tasks fits the smaller of current planned and loaded
  managed aggregate ceilings, and fits every enclosing managed service ceiling
  using only reservations belonging beneath that service.
- At each external PID ancestor, its current task count + applicable reservation
  slack + requested_tasks + 64 fits its finite `pids.max`.
- Global current tasks + aggregate reservation slack + requested_tasks + 64
  fits `min(kernel.threads-max, kernel.pid_max - 1)`.
- For the finite RLIMIT_NPROC inherited by the actual new run, current host
  tasks charged to its real UID + that UID's managed reservation slack +
  requested_tasks + 64 fits the child's enforced soft threshold. Same-real-UID
  processes can have different soft/hard limits; do not invent one UID-wide
  threshold from the observer's or another service's settings.

Task slack fixture: stable R_tasks=1088, C_tasks_A=512/C_tasks_B=256 and
requested_tasks=512 give S_tasks_A=576/S_tasks_B=832. With global current tasks
600 in both passes and global ceiling 2000, required=600+832+512+64=2008,
so refuse by eight tasks even though the managed aggregate requirement 1600
fits its 1936 ceiling. Subtracting maximum C_tasks instead would falsely fit
at 1752. An increase in unrelated task use can offset a managed task decrease;
constant global counts do not justify selecting the smaller managed slack.

The host task count is the total-entities field in
[/proc/loadavg](https://man7.org/linux/man-pages/man5/proc_loadavg.5.html),
sampled twice conservatively like other counters. Same-real-UID task accounting must
cover the applicable host user namespace; an incomplete proc view cannot prove
that predicate. Initial automatic support requires the existing host systemd
user-service namespace arrangement with observable enclosing controls. A nested
namespace/hidden task view that prevents these checks refuses auto activation;
a temporarily unreadable supported view is recoverably unknown. This profile
uses an unprivileged nonzero real/effective host UID; exempt root/capability
arrangements are unsupported auto profiles, not assumed to obey NPROC. Kernel limits
are constraints, not evidence that their entire numeric range is currently free.

Do not change kernel pid_max/threads-max, RLIMIT_NPROC or operator quotas.
Unrelated processes can race the observation; the kernel can still refuse a
later clone. Record such runtime exhaustion truthfully and retain live resource
accounting through checked cleanup. It consumes no shipping allowance by itself.

### Exact task fixtures

Planning cases enable all four table roles with their unmodified defaults,
use B=12 GiB, D=512 MiB and D_tasks=64. K=512/GiB comes from `host-text`,
not the native guest's RAM; A=11.5 GiB. Desired task count before external
caps is `64 + 11.5*512 = 5952`.

| External PID ceiling | Host raw ceiling | Desired TasksMax |
| --- | --- | --- |
| unlimited | 100000 | 5952 |
| 2048 | 100000 | 1984 |
| 640 | 100000 | 576 |
| 512 | 100000 | 448; cannot fit the default run plus non-run reserve |

Admission with planned/loaded=576, R_tasks=C_tasks=64, requested_tasks=512,
external limit=640 and no competing external tasks fits exactly. Raising current
external use from 64 to 65 without reducing other reservations refuses: 65+0+512+64
is 641. With R_tasks=576, C_tasks=100 and another 512-task request, idle reservation
slack is 476; current low usage does not admit that second request. Missing UID
accounting under a finite NPROC limit returns unknown rather than dropping it.
With only default `native-without-build` and `native-with-build` enabled,
K=128/GiB and the uncapped desired count is `64 + 11.5*128 = 1536`, not 5952.
If the actual child's soft NPROC is 1000, real-UID task use is 425, slack=0 and
request=512, the required 1001 refuses even when that child's hard limit and
a same-UID daemon's soft limit are 10000.

F1. RLIMIT_NOFILE is a per-process descriptor-number ceiling, not a cgroup-wide
count of file handles. The default host run capability is 4096 descriptors per
process, matching the existing service default. An explicit larger required
declaration must be represented in the demand revision; an explicit smaller
service ceiling cannot silently resize that requirement. Verify the required
ceiling against the actual service's enforced soft/hard limits and kernel
`fs.nr_open`; insufficient limits refuse with `descriptor-process-ceiling`.
Do not raise a kernel or operator limit to make it pass. Supported fresh service
installation sets both service soft and hard NOFILE to the greatest required
ceiling among its enabled roles. Before dispatch the trusted run supervisor
sets both child soft/hard NOFILE to that run's declared ceiling and verifies
readback. It also sets no-new-privileges and verifies empty effective, permitted
and ambient host capability sets for the unprivileged child, so raising the
hard ceiling through later privilege gain is not an accepted execution path.
Unknown enforcement remains closed. Check inherited descriptor indices as well as the
read-back limit before paid release: lowering a limit does not close already
inherited descriptors. Guest NOFILE 256 remains a separate guest capability.
See [RLIMIT_NOFILE](https://man7.org/linux/man-pages/man2/getrlimit.2.html).

F2. System-wide file handles are a pressure check, not a claimed exclusive pool.
Observe `fs.file-nr` allocated count and `fs.file-max`, taking the greater allocated
count and smaller limit in the two samples. Require
`allocated + required_process_nofile + 4096 <= file_max`; the additional 4096 is
protected host startup headroom. Ignore the historical unused-handles field,
which does not increase available capacity. Unknown or overflow refuses new
admission. Recheck in the existing admission operation; do not add an independent
FD allocator. See the [kernel file-table interface](https://docs.kernel.org/admin-guide/sysctl/fs.html#file-max-file-nr).

NOFILE does not bound every open file description's lifetime: mappings or
descriptor transfer can retain references beyond an individual descriptor.
Therefore do not claim `tasks * NOFILE` as a structural upper bound or reserve
that invented amount in the storage ledger. F2 establishes current startup
headroom, not complete future file-table exclusivity. Compensating controls are
the per-process descriptor ceiling, task/memory bounds, kernel enforcement,
bounded provider calls, observed exhaustion and checked failure/cleanup. Report
this residual limit; a later kernel-wide shortage is a runtime capacity failure,
not successful work. A stronger exclusive file-handle guarantee would require a
separate enforcement contract and is not claimed by this feature.

### Exact descriptor fixtures

- Required NOFILE=4096, loaded soft=4096, hard=4096, nr_open=1048576 passes the
  process-ceiling check. Soft=1024, hard=4096 refuses until normal service
  publication/restart/readback installs the required soft limit.
- Required NOFILE=4096 with nr_open=2048 refuses; a 256-descriptor guest limit
  cannot substitute for the host requirement.
- file_max=100000, allocated=91808, required=4096 fits exactly, preserving 4096.
  Allocated=91809 refuses with a one-handle deficit. A second sample growing
  allocated from 90000 to 92000 uses 92000 and refuses with deficit 192.
- A process already holding descriptor index 5000 while soft/hard read 4096 is
  not accepted as an enforced 4096-capability startup merely from that readback.
- Missing file-table counters are unknown, not a fabricated free count. Passing
  these checks does not independently grant execution or reserve a shipment.

## 6. Native storage producer: verified boundaries

These findings describe the source on the base of documentation PR #1916.
They are not proof that the current 1 GiB logical session charge bounds physical
growth. C2 must preserve that full charge regardless of small observed exports.

| Producer | Current boundary | What is still unbounded or unproven |
| --- | --- | --- |
| Guest runtime export | 64 MiB aggregate data, at most 4096 files and 2048 parent directories, framed/contiguous writes | Filesystem metadata and physical allocation; accumulation across interrupted publications |
| Retained generations | Successful completion prunes to the latest two `generation-*` directories | Rename precedes auth refresh and complete-pointer publication; failure/crash before pruning can retain another generation |
| Launch temporary directory | `native-vm-launch-*` inside the retained state root; normal context cleanup | SIGKILL/power loss bypasses context cleanup; another launch does not prove the old directory safe to delete |
| Native executable copy | Source stat checks a 512 MiB maximum before `shutil.copyfile` | Copy is not bounded through a held source descriptor; source growth/replacement can invalidate the stat observation |
| Codex companion | Manifest digest binding, one companion, output rejected above 128 MiB | Source `read_bytes()` is unbounded before checking length; memory peak and whole-launch disk/crash retention remain separate |
| Input assets | At most 32, bounded descriptor reads, 16 MiB aggregate | Directory metadata and coexistence with other launch outputs |
| Runtime seed/initrd | Reads existing generation up to 64 MiB; may insert a controller auth file up to 1 MiB afterward | 65 MiB seed is possible; busybox/modules/helper/job/certificate input and compressed initrd peak need a combined bound |
| Guest writable home | Inside guest RAM; host runtime/provider mounts are read-only | Guest memory is execution demand, not retained host disk demand |

Source: `scripts/native-provider-vm.py` (`runtime_seed`, `boot_entries`,
`publish_runtime_generation`, launch staging/export/completion),
`scripts/native-provider-vm-guest.py`, and
`crates/augmentagent-channel-core/src/native_state_capacity.rs`.

The existing executable/companion byte-limit violations are tracked separately
in [#1924](https://github.com/nolanmak/Jarvis/issues/1924), with the TDD fix in
[PR #1925](https://github.com/nolanmak/Jarvis/pull/1925), source `dbc7ef09`.
Five behavioral failures reproduce against the unchanged-copy extraction
`5e562e03`; the native-provider suite (32 tests under umask 0002) and provisioning
suite (9 tests) pass with bounded descriptor streaming. A companion-test
permission false positive was reproduced and corrected with a precise size
refusal assertion. That fix is under review, not deployed
or proof of complete native growth/custody. It does not close C0/C2.

The aggregate home-seed violation is tracked in
[#1926](https://github.com/nolanmak/Jarvis/issues/1926) and fixed separately in
[PR #1927](https://github.com/nolanmak/Jarvis/pull/1927), source `4a958e57`.
Three overflow regressions fail on main `416f392b`, including the real 64 MiB
boundary. Final selected home bytes are checked after credential selection;
exact fit, smaller replacement and preserved refreshed auth remain supported.
All 26 native-provider and nine provisioning tests pass locally. CI, independent
review and rollout remain pending. This bounds final seed data, not metadata,
combined initrd inputs/peak, interrupted-launch accumulation or whole-launch
physical growth. Those C0/C2 obligations remain open.

Inspected Git blobs: native host `8611d1722f3117a126c704c32567739c3454c839`,
supervisor `d30dfa6bbdcbca8aa36d87b84fefb382b91f5658`, run declarations
`73b0fe627e702dab955f39d5e47735ca424c5bf9`. Later source revisions must be
rechecked rather than inheriting these findings as current evidence.

Required producer behavior before C2 can claim bounded growth:

- Declare complete active peak by filesystem, including simultaneous binary,
  companion, assets, initrd, export staging, metadata and publication overlap.
  A source-stat maximum alone is insufficient; bounded held-descriptor copies
  and checked output accounting must enforce the declared producer limits.
- Keep the current full durable session charge. Add separately declared active
  growth where the session charge does not establish its coverage; no credit
  from current `du`, sparse extents, age or a missing PID.
- Persist an interrupted launch's unresolved charge before authorizing another
  writer. Do not repeatedly spend one ephemeral reservation while orphaned
  files accumulate. Unresolved cleanup stays charged/held until the existing
  checked retirement/custody protocol proves the precise release.
- Integrate durable native commitments in the existing global per-filesystem
  admission authority. Existing v1 flock claims are process-scoped and reclaimed
  when their locks are free; they cannot represent retained session lifetime.
  Mixed-version admission must refuse unsupported durable claim formats rather
  than let an old reader reclaim them as stale process claims.

Those are required behaviors, not an approved new ledger format. Exact growth
budgets, inode/block support, durable representation, writer compatibility,
lock ordering, publication and release proofs remain C0 work below.

### Existing shared-authority boundary and reboot lifetime

Source inspection identifies two requirements for the remaining shared-storage
contract. `storage_reservations.rs` lives in the CLI and its admission operation
also invokes CLI `build_storage_ci_holds.rs` reconciliation. Native constructors
live in channel-core, which cannot import the CLI. Lift the existing common
admission/claim/cleanup-fence implementation into the shared lower layer and
keep CLI adapters; do not duplicate its lock/scan arithmetic in native admission
or omit the CI uncertainty fence. Exact public/opaque APIs remain C0 work.

`storage_leases::lock_directory()` uses `/run/user/UID` or `/tmp`. Those are
temporary coordination locations, not durable retained-session accounting.
Retained commitments must persist outside them and be reconciled by the same
admission authority before writers restart after reboot. Existing flock claims
cannot supply that lifetime: the v1 reader deletes unlocked records before
parsing their version. Merely writing a v2 native row into that directory is
therefore unsafe for both reboot and old-reader compatibility. C0 must define
the persistent backing, compatibility/startup fence and original release proof;
C2 must test an old reader, lost locks, reboot and unresolved retirement through
the actual shared entry points. No production registry or limits were changed
by this inspection.

## 7. Shared storage accounting and durable native commitments

S1. Extend the existing admission authority; do not create a second native
allocator. Move its lock acquisition, claim inventory and CI uncertainty guard
into a shared channel-core storage module, with CLI adapters for current build,
snapshot and prune callers. All these callers and native constructors use the
same per-UID admission lock and arithmetic. The shared module must retain CI
hold reconciliation before mutating admission, including its exact kernel
cleanup proof; native admission cannot bypass it because no CLI callback was
installed. Read-only inspection uses a separate nonmutating inventory API.

The consumer inventory also includes Python `codex-tool-bridge.py`:
`BuildScratch` currently budgets persistent sparse VM caches, and `CommandStorage`
holds legacy command/update locks without publishing a Rust reservation. Those
are not already part of a combined claim total. The original Rust bridge owner
must adapt those operations to the common authority and carry the original
storage binding to the actual VM command. Do not implement a second independent
SQL reader/allocator in Python or accept a policy JSON flag as a held allocation.
Exact bridge request/descriptor APIs remain C0 work before C2 implementation.
The compute retention publisher in `code_mode/compute/retention.rs` also writes
`compute-artifacts/usage.json` (`schemaVersion=1`, `reservedBytes`) before task
leases; the Python allocator reads that private inventory under the directory
lock. It is an existing local accounting input, not already a global claim.
The final consumer/migration matrix must account for its allocator and sweeper
as well as the VM image allocator; neither can be omitted or counted twice.
Auto mode replaces the bridge's managed 24 GiB aggregate scratch-budget default
with common per-filesystem accounting, rather than leaving another fixed pool
to block a larger host. Preserve explicit operator scratch ceilings as additional
constraints, the declared per-cache demand and the existing scratch free-space
floor (default 20 GiB). Fixed-mode semantics remain unchanged until supported
migration; ambiguous legacy provenance cannot be guessed from the number 24.

S2. Retained commitments belong in the existing native root database, alongside
its ownership/retirement records. Add versioned commitment tables there; do not
copy the same charge into a workflow accounting ledger or treat directory count
as a second authority. A per-UID durable root registry lets the common admission
owner discover every participating root, including an unselected old provider
root. This registry stores root membership/identity, not a second commitment
balance. Native tree/owner inventories detect missing or contradictory records;
they cannot silently reconstruct a release or discount a charge.

The registry has one deterministic location under the home directory obtained
from the operating system's account record for the effective UID:
`.local/state/augmentagent/storage-roots-v1/`. Ambient HOME/XDG overrides do not
split this global registry; per-app state remains configurable. Validate held
owner-private non-symlink registry directories/files. An unavailable or replaced
account-home/registry is unknown, never an empty inventory. Its native-root
entries and native database rows survive reboot; the temporary admission lock
and process claims may be recreated, but cannot supply their persistence.

Each registry entry binds a generated root UUID, canonical root path, original
root/database/anchor identity, expected native schema revision and filesystem
identity. The registry is append-before-allocation: publish/sync root membership
before the first charge or writer. Missing root/database or identity disagreement
keeps its commitments unknown/held and refuses new shared admission. A selected
policy changing roots does not unregister the old one. No registry retirement
is allowed until original checked retirement establishes no remaining native
commitment or unresolved writer; keep a root tombstone rather than reuse its UUID.
The exact filesystem/anchor identity encoding is still a storage-observation
gate below; these fields alone cannot recreate execution or cleanup authority.

S3. Each commitment has a globally unique allocation ID, root UUID, original
native owner/launch binding, policy and demand revisions, immutable per-filesystem
byte/inode demand, replay digest and state. Byte counts must be nonnegative
SQLite integers representable without overflow; unsupported magnitude or a
checked-sum overflow yields unknown. The native database is the sole durable
balance for these rows. Version migration and table constraints must reject
duplicates, changed replay parameters, unbound rows and unknown state values.

There are two distinct commitment kinds:

- `retained`: the full existing 1 GiB logical session charge, from creation until
  checked retirement and deletion complete. A resume does not create another
  retained charge. This is an accounting promise, not a demonstrated kernel
  quota or credit for materialized data.
- `active-growth`: the producer's complete additional per-filesystem peak for
  one launch, including state staging and other declared output paths. This
  charge remains durable after process loss until checked cleanup/publication
  reconciliation settles it. Section 6's full producer budget is still required;
  an unverified peak is unknown, never zero or assumed covered by the 1 GiB row.

For a build-enabled native run this includes its declared sparse build-cache
growth and command scratch/source snapshots, even while the model is thinking.
The current bridge default is a 12 GiB cache, not the native 1 GiB retained
charge. Reserve its full declared future growth before releasing that run; do
not discount sparse materialization or wait until a paid coding turn requests
its first build. A no-build route does not reserve a nonexistent build cache.
An existing per-command claim and its original durable parent coverage must be
correlated by the same allocation/component binding, not counted twice or
excluded from an unverified caller-supplied ID. Acquiring the build/prune lifetime
lease for each actual command remains separate from the growth commitment.

States are `pending`, `held`, `release-pending`, `released`. The first three
count their full immutable demand; only `released` counts zero. Loss of a flock,
PID exit, daemon restart, age, small file size or a cleanup log changes none of
these balances. Process-scoped build/snapshot claims keep their existing lifetime;
CI uncertainty holds still prevent premature reuse of uncertain process claims.

S4. For each actual filesystem f, define:

- F_f: current user-available bytes on the held, verified filesystem, using the
  smaller coherent observation. This is already net of materialized allocations.
- H_f: the greatest applicable configured free-space floor of the participating
  native/build/snapshot paths on f. Preserve the existing 15 GiB default floor
  and native scratch's default 20 GiB floor where that profile applies, plus
  any larger explicit floor. Floors are not pooled across devices and are
  not added repeatedly for aliases of the same filesystem.
- P_f: sum of other active process claims' full declared growth on f, each
  allocation ID counted once. An existing build's claim and its parent's inherited
  claim are one allocation, not two. Never subtract its materialized usage.
- D_f: sum of every pending/held/release-pending native commitment on f across
  all registered roots, each allocation ID counted once.
- Q_f: this operation's new full demand, consisting of the retained charge when
  creating a session plus its separate active-growth demand. Resume has no new
  retained charge. Exclude only an original held allocation being replayed with
  the same immutable digest; a caller-supplied ID cannot hide someone else's row.

An idempotent accounting replay returns the existing charged row with no new
Q_f; it does not itself permit another physical write. An original writer whose
live custody/binding is still fully verified may continue within its already
reserved active peak. Its continuation check excludes only its own active-growth
component from D_f and instead includes the producer's verified remaining
physical-growth upper bound U_f for that component; it still counts retained
rows, other writers and any uncovered new demand. Without a producer proof of
less remaining growth, U_f is the full original component demand. In particular,
a directory-size/allocated-block sample alone cannot justify U_f=0. Every other admission
continues to count that original active-growth row in full. This is original
lease reuse, not a materialized-usage rebate or a reduction of the durable row.
Without the original proof, replay remains charged/held and cannot qualify an
execution path through a serialized allocation ID.

Admission requires `F_f >= H_f + P_f + D_f + Q_f` on every affected filesystem,
plus the analogous checked inode/quotas and logical-image predicates once their
profile is specified. If two paths refer to f, count the same allocation once;
separate components of one operation on f sum into Q_f. The same allocation
may carry different demand on different filesystems. Archive free space never
funds growth on a state/build device. An unknown operand makes the result unknown.
Per-root fixed byte/count overrides and the existing owner/source/authorization
constraints remain additional predicates. Global headroom never overrides a
selected root's explicit ceiling.

For a proposed native launch on one state filesystem, the current auto retained
aggregate envelope across all registered roots is
`floor(max(0, F_f-H_f-P_f-D_other_f-G_f) / 1 GiB) * 1 GiB`, where
D_other_f excludes all retained rows but includes unresolved active-growth rows
and G_f is this launch's new active-growth peak. The existing retained rows
still consume that envelope; do not add them to the result or apply the whole
global envelope independently as each root's new policy ceiling. This displayed
envelope is not pre-reserved as a native pool: only actual rows are committed.
If a native operation needs multiple filesystems, the full vector predicates
decide it; one displayed state budget cannot stand in for those checks.

S5. The same arithmetic applies when a builder or snapshot is the new writer:
its own existing allocation is excluded only for original in-bounds continuation,
not to evade a new claim. Native D_f remains charged to other writers. A native
VM command uses the original active-growth component covering its cache/scratch;
it does not acquire another promise for that same growth. Uncovered paths or
additional demand require ordinary new admission. Continuing a covered command
still requires its verified remaining U_f to fit current headroom; the original
promise does not excuse a known physical deficit after unrelated writes.
Standalone build peak is
separate from H_f, as in current
admission (default 10 GiB growth plus 15 GiB floor). Snapshot peak remains the
existing raw/compressed overlap formula `2*db_bytes + floor(db_bytes/10) +
binary_bytes`, added to the applicable floor and other claims. Do not count a
new writer's peak both inside its floor and again as Q_f. Existing retained
promises can block an unrelated future build; report that resource deficit rather than
erase promises, capture the entire theoretical native pool, or promise future
build capacity that was never reserved. Workflow fairness and checked retirement
must recover eligible work through the existing scheduler.
For an already admitted build-enabled native run, S3 has reserved its declared
cache/scratch peak before model release; subsequent covered commands reuse that
promise and the existing command-scoped build/prune lease. They do not discover
an unreserved 12 GiB requirement only after a paid turn has started.

### Exact storage arithmetic fixtures

These are synthetic declared-demand fixtures, not a claim that the native
producer currently proves G=2 GiB. All values below are GiB; every physical,
identity, inode, quota and logical-image predicate is otherwise known/pass.

| New operation | F | H | Other P | Existing D | New Q | Result |
| --- | --- | --- | --- | --- | --- | --- |
| Native create, retained 1 + active 2 | 39 | 15 | 10 | 8 | 3 | fits; total 36 |
| Same operation under pressure | 35 | 15 | 10 | 8 | 3 | deficit 1 |
| Resume, active 2 only | 35 | 15 | 10 | 8 | 2 | fits exactly |
| New build, growth 10 | 39 | 15 | 0 | 8 | 10 | fits; total 33 |
| New snapshot, db=2 and binary=0.5 | 28 | 15 | 0 | 8 | 5046586572 bytes | fits; total 29742648524 bytes |

The snapshot row uses integer division of byte counts, not decimal GiB
arithmetic; its peak is slightly less than 4.7 GiB.

For the first row, the total retained envelope is 12 GiB
(`39-15-10-2`), with 8 GiB already committed and four retained slots of current
arithmetic headroom. It is not 20 GiB and reserves none of those four slots yet.
An unresolved prior 2 GiB active-growth row lowers that envelope to 10 GiB;
a resume also needs its new active-growth claim despite its existing retained row.

Two native roots each holding 4 GiB on the same device contribute D=8 GiB,
not 4 or 16. Repeating the state path through an alias changes no total. If a
launch needs state demand 3 GiB on f and build growth 10 GiB on g, evaluate each
device independently. With F_g=24, H_g=15 and no other claims, g refuses with
deficit 1 GiB even if f has 100 GiB spare. Two concurrent new 3 GiB native
allocations against F=35,H=15,P=10,D=6 serialize: the first fits (34), the second
observes D=9 and refuses (37). They cannot both consume the same three GiB.

Continuation fixture: F=24,H=15, retained D=8, own original active-growth=2,
other P=0. With a producer proof that remaining U=1, the still-qualified original
writer fits exactly at 24. Without that proof U=2 and it refuses at 25. A
different new native create still counts D=10 and Q=3 and refuses at total 28.
For a native cache on a separate scratch device with F=24,H=20 and its own
qualified 12 GiB active component, a verified remaining U=4 fits exactly at 24;
if all 12 may still be needed, it refuses at 32. A different 12 GiB cache still
counts that existing full promise and requires 44. The producer proof here is
a synthetic fixture input, not a claim that current image block counts establish it.

S6. Native creation/replay under the existing root state lock follows this order:

1. Hold existing provisioning lock when changing policy/root membership; hold
   native state lock for native mutation, then acquire the common storage
   admission lock. Existing target/snapshot lifetime locks retain their present
   outer order. The common storage lock never acquires a native state lock,
   waits for a target/run lock, awaits a VM or invokes network/model work.
2. Validate registry/root/database identities and current policy/demand/replay
   binding, reconcile existing uncertainty guards, collect the full inventory
   and coherent filesystem observations, and evaluate S4 for all devices.
3. Commit/sync the exact `pending` native rows in the existing root database
   before authorizing physical growth. Then release the common lock. No local
   runtime/source/alias publication can precede its charged intent.
4. Construct the runtime using the existing original ownership/source/input
   protocol, persist/sync the allocation reference with those native records,
   then mark its rows `held` under the same state/common-lock order. This
   cross-store sequence does not grant model execution. Before release, actual
   resource admission still validates the original source/resource/input handles.
5. Lost acknowledgement reopens the same allocation/replay digest. It never
   creates a replacement ID to escape debt. A crash at any intermediate point
   leaves pending/held full charges; unknown original ownership remains held.

Read-only registry/database scans during common admission take no native state
lock and have zero SQLite busy wait. Capacity-writing native transactions require
the common lock, so no cooperating allocation can change mid-scan. A busy or
invalid database produces a recoverable storage outcome; do not wait while
holding the global lock or omit that root to obtain a result. Durable database
transactions require full synchronization and directory synchronization at
creation/publication boundaries. Exact schema/SQL and replay-record binding must
be checked by the C0 migration contract before implementation.

S7. Release is a separate, original-proof-consuming operation. Record
`release-pending` while still charging the full row, obtain the existing native
qualified retirement/quiescence proof, unlink only the precisely owned artifacts
through that protocol, sync affected parent directories and complete its durable
retirement publication. Only then mark the corresponding rows `released` under
state/common locks. Retained aliases/reference cleanup must precede freeing the
session row. A successful resumed turn may release that launch's active-growth
row only after original descendants and temporary growth are checked settled;
its retained row remains held. Committing a new generation does not itself prove
all launch scratch/diagnostic writers gone.

Crash between physical deletion and row release overcharges until original
reconciliation proves the release; it never undercharges. Path absence alone,
metadata rewritten after losing original handles, a copied completion JSON,
or an old unqualified legacy retirement record is insufficient release proof.
No new table, registry field, event or replay ID creates cleanup authority.

S8. Mixed-version/startup fencing is load-bearing. In particular,
old v1 readers delete unlocked process claims before parsing their version, and
old native admission never consults the shared registry. A new table/row type
or a documented “upgrade every writer” convention does not structurally fence
them. Before auto activation, the supported migration must verify the complete
managed writer set, enforce a compatible minimum version/startup barrier across
reboot and rollback, and positively settle incompatible active writers. Unknown
compatibility keeps activation blocked. S9 specifies the proposed negative
fences and handoff requirements; their original-proof APIs, publication journal
and complete managed-consumer integration still block implementation handoff.

S9. Use a monotonic per-UID minimum storage protocol of 2, persisted with the
durable registry/control plane before publishing the first protocol-2 fence.
This floor is independent of auto versus fixed sizing. Switching back to fixed
does not permit an allocator that ignores retained commitments. Once published,
normal rollback cannot erase the floor, return lock permissions to legacy mode,
or restore older authority/accounting. Prepare a retained, compatible recovery
binary/configuration through the normal snapshot/updater path first. A failure
after floor publication is a publication/recovery operation, not an untouched
failure eligible to re-enable old code.

The supported initial permission-fence profile is an unprivileged, nonzero
real/effective UID on local Linux filesystems supporting read-only exclusive
flock (the current ext4 storage and systemd-user runtime filesystem arrangement).
Root, privilege-bypassing capabilities, remote-lock semantics or unknown file/
namespace identity do not qualify through this profile. This is a negative
compatibility mechanism, not exclusive physical disk protection against unrelated
applications or deliberate owner bypass.

#### Negative old-client fences

- Preserve the original shared admission-lock inode. Through its held descriptor,
  set/verify owner-read-only mode 0400 and synchronize it. Protocol-2 code opens
  that checked inode read-only and takes the same exclusive flock. Legacy
  `storage_reservations::open_private` requires read+write and therefore fails
  before admitting a new writer. Never unlink/replace an owned lock to obtain
  an uncontended one.
- Publish a private reserved compatibility marker in the existing process-claim
  namespace, with a reserved valid UUID `.reservation` filename and mode 0400.
  Protocol-2 inventory recognizes only the exact marker bound to the persistent
  protocol/registry and current namespace identities. Legacy claim scanning opens
  records read+write before reclaiming free flocks, so it fails on that marker
  instead of deleting an unlocked durable row. A generic unknown record cannot
  be skipped as a marker. The exact marker encoding remains part of the final
  schema; an ID alone cannot authorize exclusion or parent-claim reuse.
- Apply the same checked read-only-lock protocol to each participating native
  root's existing `state.lock`, which legacy native admission/retirement opens
  read+write. New native policy format/loader also refuses old code, but this
  inode fence covers a caller that cached a v1 policy before the change.
- The Python scratch allocator bypasses those lock inodes. Its existing
  `_open_root` accepts only exact owner mode 0700. The proposed protocol-2
  scratch-root discriminator is mode 01700 on the same checked directory:
  owner access remains, while the old allocator refuses before image creation.
  Compatible code must explicitly validate the new mode rather than silently
  chmod it back. A private `protocol-2` child holds new image/session paths;
  its name does not match the legacy Rust sweeper's immediate-child
  `jarvis-vm-session-` filter. Do not move or rename old live session paths.
  This fences fresh old Python allocation only: it neither revokes preopened
  directory descriptors nor fences Rust compute allocation or every cleanup
  path. Those consumers still require the complete startup/handoff matrix.
- Guard both legacy global lock-directory selections (`/run/user/UID` and its
  `/tmp` fallback), or refuse activation if either cannot be safely fenced.
  New compatible clients use one bound coordination namespace and refuse a
  missing/replaced namespace rather than silently starting another allocator.
  Runtime-directory loss is not permission to use an unfenced fallback.

#### Positive handoff, not an absence scan

Permissions do not revoke a descriptor already opened read+write. A disposable
non-root kernel experiment verified that such a descriptor still writes/locks
after chmod 0400. The marker protects legacy claim-scanning paths, but is not
proof that every old native/prune/controller path has stopped.

Before authorizing protocol-2 allocations, the migration controller must hold
the startup/deployment exclusion and obtain an original, checked compatibility
handoff covering every supported
managed consumer instance: daemon/worker controllers, native owners, CLI work
controllers, builders, snapshots, pruners, and the Python bridge/cache helpers.
Cover their controller processes and delegated descriptors, not only guest or
run-child cgroups. Binary version, quiet PID, expired lease, zero model calls,
an unlocked flock or a process census alone is insufficient.

This handoff is an opaque original-proof capability, not a deserializable
`all_writers_stopped` flag or a result minted from callback-selected booleans.
The migration may wait for normal guarded drain/recovery outside storage/native
locks; it may not kill sessions, bypass owner holds or reclaim unknown state.
Existing native owner registration history (`workflow_native_owner`, registered
before dispatch) contributes completeness/correlation evidence but does not
itself prove quiescence or authorize retirement. Missing producer/instance
coverage keeps activation prepared/held with capacity-protocol-incompatible.
Do not set owner retirement fields, release retained charges or call native
retirement/pruning merely to make this handoff appear complete. Normal actual
workflow retirement remains a separate original-proof-governed operation.

Use a staged cutover rather than assuming all old consumers obey a new flag:

1. Prepare the compatible recovery binary and complete entry-point/bootstrap
   plan; publish the guarded startup selection before the persistent protocol
   floor/fences. This ensures reboot during a partial migration cannot restart
   an old managed writer without bootstrap. A source-backed old entry point not
   covered by the plan blocks this step; names/version flags are not coverage.
2. Persist the protocol floor and install/verify the negative fences under the
   original native/common lock inodes. Release those locks. Fresh legacy opens
   now fail, while already-admitted/preopened old instances may drain under their
   original fixed-mode authorization. No new protocol-2 allocation is permitted.
3. Obtain the positive original handoff, including controllers with preopened
   descriptors, through normal guarded drain/recovery. Do not wait for them while
   holding storage/native locks. Binary capability alone does not change an
   already-prepared incompatible invocation into a compatible one.
4. Reacquire/revalidate the original native/common locks, handoff, closed consumer
   set and all fence identities. Only now import final legacy balances and publish
   the compatible policy/schema/registry operation. Then complete ordinary guarded
   reload/readback before permitting new protocol-2 work.

Keep the same migration operation across lost acknowledgements and known waiting
phases. Pending compatible drain is a journalled publishing step, not permission
to reopen legacy locks or recompute the plan. A failed/unknown handoff remains
held; do not “try a v2 allocation” as a test of whether an old waiter exists.

#### Boot and rollback cutover

The persistent minimum protocol survives reboot; `/run` locks/markers do not.
The supported service/CLI/updater entry points must run compatible bootstrap
before any managed allocation. Bootstrap verifies the persistent registry and
publishes/verifies the negative fences in both legacy coordination locations,
then resumes the same journalled migration. Recreating fences alone cannot
permit admission: positive handoff, final balance import, compatible policy/
registry publication and actual loaded readback must all be durably complete
and revalidated first. An interrupted cutover remains held at its original step.
Its source/binary binding and unit ordering
are checked in the final C3 matrix, not inferred from a helpful executable flag.
It performs no model work and does not convert stored records into a live
execution grant. A failed bootstrap leaves managed admission blocked with precise
diagnostics; it cannot select a clean empty runtime directory as recovery.

New code revalidates these negative fences and namespace identity on each
admission. Fixed mode, updater repair and resource-only rollback still obey
the minimum protocol. Unknown/corrupt registry or fence data refuses admission;
neither an old snapshot nor a mode toggle resets it.

### Compatibility fixtures and evidence scope

- Non-root disposable-file probe: a fresh legacy O_RDWR open of mode 0400 fails;
  a checked O_RDONLY descriptor obtains exclusive flock; an already-open O_RDWR
  descriptor remains valid. A second descriptor also respects an existing
  exclusive holder. These observations were reproduced in both a storage-temp
  directory and the private systemd-user runtime directory using
  `python3 docs/autoship/probes/storage_compat_permissions.py`.
  This samples the current temporary/runtime mounts; it does not certify every
  supported storage filesystem, remount profile, or actual deployment namespace.
- A mode-0400 reserved claim marker rejects the legacy read+write open before
  its free-flock deletion logic. This kernel-open experiment was reproduced;
  actual old Rust/native/bridge entry-point acceptance remains required by C2/C3.
- The same probe imports the repository's actual old `BuildScratch`, records
  its SHA-256 (refusing an unreviewed source revision), and verifies mode 01700
  makes `open()` return
  `build_scratch_unavailable` before `_require_space` or image allocation.
  A deliberately preopened directory descriptor remains usable. Both locations
  pass, and no VM image is created. This tests only the legacy root validator,
  not compatible allocation, Rust sweeping, or complete migration readiness.
- A retained preopened legacy descriptor/controller prevents handoff even if its
  model count is zero or its run-child cgroup is empty. Fence installation alone
  cannot mark that case loaded/compatible.
- Reboot removes temporary coordination but keeps retained rows and protocol
  floor: no managed writer starts before compatible bootstrap recreates/verifies
  both fences. Fallback replacement or a foreign directory remains unknown.
- Switching auto to fixed or selecting an old rollback binary does not restore
  mode 0600, delete the marker, drop the protocol floor or hide native charges.
- A legacy root with incomplete owner history/proofs remains held with its data
  and full charges preserved. A bound-owner schema uses its actual qualified
  migration handler; the legacy budget writer from #1913 is not such a handler.

These decisions specify the required mechanism and tests. They do not claim the
handoff producer, marker/schema, complete startup matrix or bound-owner migration
handler is implemented or closed. No production permissions were changed by the
kernel experiment; it used only disposable temporary directories.

## 8. Configured mode, publication and runtime adaptation

L1. Supported fresh Linux deployments persist `mode=auto` by default. Explicit
caller resource settings, including maximum limits and M3's protected-memory
reserve minimum, are recorded with property, value and provenance, even
when numerically equal to an old default. Absent arguments are not implicit
8 GiB/16-session, 8G/10G service or 24 GiB scratch overrides. A supported machine
that cannot fit a complete run remains configured auto and reports the limiting
demand. Temporarily unavailable evidence also keeps auto configured with new
admission blocked. Unsupported activation refuses explicitly; it never silently
falls back to a fixed policy and calls that automatic sizing.

Existing deployments remain fixed until the supported explicit enable-auto
operation binds their current policy/scope revisions and management provenance.
The owner's current request authorizes that supported operation after normal
review/rollout; it does not require another choice of arbitrary bytes. Preserve
known explicit ceilings as overrides. Ambiguous legacy properties require the
operation's explicit, hash-bound adoption of named resource properties; do not
classify them as installer defaults from their names, headers or numeric values.
The exact schema and operation/replay encoding remain C0 gates below.

L2. Keep these facts separate in persistent/readback state:

- Configured mode, explicit overrides, policy revision and predecessor digest.
- Desired derived service/slice/scratch settings and their observation/demand/
  provenance revision, before any file or service change.
- Published candidate file identities/digests and operation ID, before reload.
- Actual loaded worker/daemon/aggregate kernel limits and selected configuration,
  bound to process birth/boot and the same current operation/revision.
- Current admission outcome/deficits under those actual limits and live claims.

No scalar `enabled` or `applied` substitutes for all five. A larger desired
budget is not usable before enforced readback. A configured auto mode with a
capacity deficit remains blocked, rather than reported fixed, disabled or healthy.
The complete managed output set includes both services and the shared slice;
reading only the daemon cannot certify worker activation.

L3. Publication uses the normal retained deploy snapshot and updater. The
capacity operation record has distinct states `prepared`, `publishing`,
`publication-uncertain`, `published-awaiting-load`, `loaded`, and `failed-before-write`.
`prepared` contains the immutable candidate, expected current revisions/hashes,
affected resource properties and snapshot/compatibility reference. Persist/sync
`publishing` before the first changed file. A failure after any write/rename/sync
whose result is unknown becomes `publication-uncertain`, not a fresh failure
eligible to recompute another candidate.

Reconciliation compares every affected file's original/candidate identities and
digests while preserving the original operation. If all candidate files are
durably present, continue to `published-awaiting-load`. A proven untouched source
set may terminate `failed-before-write`. Unexplained partial or contradictory state stays
uncertain and fences new auto admission until that same operation is reconciled
through its recorded write plan. The detailed multi-file step journal must be
specified with the final schema; this state list alone does not authorize
blindly rewriting an observed mixed set or rolling back authority.
An intact journal may identify a known partial publishing step, such as S9's
fences-installed/waiting-for-drain phase. Resume that guarded step of the same
operation; it is neither loaded success nor an unexplained lost acknowledgement.

Use existing deployment pin, active-work, memory-pressure and restart-budget
guards. `scripts/check-for-updates.sh` already distinguishes policy selection
from executable updates and defers reload at guarded boundaries. Extend that
path; do not force-restart an active worker from a capacity poll. For configuration
increases and ordinary recovery, the worker drains to its existing tick/sweep
boundary. Any migration needing incompatible-writer teardown must satisfy the
separate S8 positive-quiescence/startup fence before activation.

`loaded` requires current actual worker and daemon receipts plus aggregate
kernel readback covering the whole candidate. A daemon restart acknowledgement,
file rename or worker PID alone is insufficient. Missing/mismatched readback
stays `published-awaiting-load` or uncertain as appropriate; it cannot fabricate
recovery. Failed delivery of an update has no bearing on publication/admission.

L4. Rollback coverage extends the existing deploy snapshot format with versioned
capacity resource projections, source property/file identities, provenance and
compatibility metadata. Capture the relevant resource values, not an assumption
that today's database/binary snapshot already covers them. Do not create raw
database copies or separate rollback directories. Snapshot refusal keeps the
migration unstarted; no retention/floor override.

Rollback is a new checked resource-policy operation using that projection and
current authority. It is not restoration of an old native/workflow/accounting
database or an old whole policy containing grants. Preserve current credentials,
security revocations, native owners, workflow holds and charges. Overlay only
owned resource properties onto current compatible configuration. Revalidate
current identities, enclosing constraints and commitments; a snapshot's old
8 GiB ceiling cannot be imposed over 9 GiB of current retained promises.

Only a binary/configuration combination supporting the current storage/native
protocol may load this rollback. An incompatible old binary or unknown schema
compatibility refuses explicitly and requires compatible roll-forward. Never
erase durable rows or the minimum-protocol fence to make rollback succeed.
The final compatibility matrix and snapshot projection schema still block C3.

L5. Runtime observation reuses the worker's existing unpaid reconciliation loop,
whose interval is capped at 300 seconds with missed ticks skipped. Observe at
startup and on each pass; no additional allocator or independent paid retry loop.
Every actual allocation still performs its own fresh locked check and never
waits up to that interval to reject known pressure or a changed identity.

Service/slice desired limits derive from stable host/external constraints and
validated demand, not noisy MemAvailable/free-byte samples. Live availability
changes admission and displayed deficits; they do not rewrite service files on
every tick. Dynamic retained-disk headroom is an observation, not a preallocated
pool or a new persistent ceiling for every byte of filesystem fluctuation.

L6. Apply shrink immediately to new admission using the smaller current envelope,
even while an older larger kernel limit is loaded. Preserve existing commitments.
Do not publish a new managed hard ceiling below its still-live reservations:
defer that decrease until checked releases make it fit. External/kernel shrink
may already affect running work; report overcommit/runtime exhaustion truthfully,
without killing sessions or erasing charges to claim the new size fits.

A desired managed increase needs two consecutive known, identity/policy/demand-
consistent observations of the same candidate, at least 60 seconds apart on the
same boot. Unknown, changed candidate, changed revision or intervening shrink
resets that stability evidence. Then submit one normal updater operation; its
guard/readback states remain authoritative. Persist the candidate/observation
identities for deduplication, but restart does not turn old samples into fresh
increase evidence. Initial supported installation uses its direct checked
provisioning operation; it does not fabricate two historical samples.

Ordinary admission recovery from transient contention needs the first current
known sample that fits; the two-sample rule applies to increasing managed
configuration, not to hiding existing usable capacity for another interval.
If pressure returns between observation and execution, the fresh original
allocation guard refuses before paid release. Neither transition is shipment.

L7. A resource wait uses unpaid observation/reconciliation, without reopening a
productive attempt every hour against unchanged evidence. The worker may invoke
the existing authority's unpaid, original-proof-checked hold/cleanup reconciliation
on that pass; this is distinct from a pure status inspection. Otherwise an old
cleanup hold could prevent the very paid attempt whose admission used to clear
it. No release follows from time or a free flock alone. Preserve its existing protected
workflow checkpoint, generation, owner controls, demand and binding. Its wake
fingerprint binds policy/demand/loaded/resource identities plus the typed
known/unknown/blocked/available outcome for that complete next action; raw
byte/time churn alone does not change readiness or queue an attempt. Stable
blocker details remain visible even when no new workflow event is warranted.

When a current outcome becomes available, request ordinary fair scheduling;
eligible work advances within the parent epic's two reconciliation cycles,
subject to actual capacity and mandatory checks. Observation is not a direct
dispatch callback. Holds, pending migration, invalid source/input and provider
cooldown remain independent predicates. Provider/review/CI retry backoff does
not get reset by a free-space change. Capacity observation/deferral consumes
zero paid model calls and zero shipping allowance; model work starts only after
the existing full execution boundary authorizes it.

### Exact lifecycle/fake-clock fixtures

- Loaded B=12 GiB, new current B=6 GiB and existing complete reservations=6.5
  GiB: new admission blocks immediately. Loaded limit remains unchanged pending
  checked releases; no process is killed and no half-GiB charge disappears.
- A larger candidate at t=0 and t=59 seconds is not increase-eligible; the same
  bound candidate at t=60 is. Unknown at t=30 resets evidence, so another known
  sample at t=60 is the first, not the second. A candidate changed at t=59 is
  also the first sample of the new candidate.
- Reconciliation at t=0 and t=300 with an unchanged increase candidate satisfies
  stability. Updater guard refusal leaves desired/published/loaded distinct and
  queues no second candidate under a new operation ID to evade the guard.
- Pressure blocked at t=0, known available at t=300: request fair work at that
  pass without a second recovery sample. It must still pass current admission
  before paid release. A reappearing deficit at execution yields a capacity
  outcome with no model call or allowance consumption.
- Increasing free space by one byte while the complete queued demand remains
  blocked creates no productive retry. Changing worker identity invalidates
  old evidence; it cannot itself become a recovery/merge/deployment event.
- A published policy whose worker has not reloaded stays awaiting load even if
  the daemon restarted successfully. Restart/reopen during uncertain multi-file
  publication reconciles the original candidate; it does not reset accounting.
- A rollback requesting 8 GiB against 9 GiB of retained commitments refuses and
  leaves configuration/security/accounting intact; rollback to a binary that
  ignores durable native claims refuses independently of numerical capacity.

## 9. Versioned CLI/private Discord projection

N1. Introduce `CapacityStatusV1` as observation/projection data, never an
execution or cleanup grant. Its JSON object has these fields:

| Field | Type and meaning |
| --- | --- |
| `schema_version` | Integer 1 |
| `configured_mode` | `auto`, `fixed`, or `unknown`; control-plane intent, not proof of loaded mode |
| `overrides` | Bounded applicable override inventory defined below, including evidence completeness |
| `configuration_state` | L3 state, `unchanged`, or `unknown` |
| `outcome` | `available`, `blocked`, `unknown`, `unsupported`, or `not-evaluated` for the full next action |
| `demand_state` | `selected`, `not-selected`, or `unavailable` for the inspected next action |
| `demand_kind` | P2 role identifier for the selected complete declaration, or null |
| `worker_evidence` | `current`, `missing`, or `stale`; from actual selected worker evidence |
| `observed_at_ms` | UTC milliseconds for this observation, or null if not observed |
| `measurement_age_ms` | Nonnegative age of the measured result, or null when unverified; never an execution freshness waiver |
| `predicates` | Typed rows defined in N2 |
| `omitted_predicates` | Nonnegative count of checked rows omitted from the bounded projection |
| `reason_codes` | Sorted, deduplicated allowlist values defined in N3 |
| `next_action` | `await_capacity`, `await_configuration`, `inspect_private_status`, or `none` |
| `next_check_at_ms` | UTC next observational eligibility, or null when unknown/not applicable |

All fields are required in version 1, including explicit nulls. Available means
the authoritative observation checked the whole declared resource vector and
configuration prerequisites, not merely the rows displayed. It is still subject
to current owner controls and actual fresh execution admission. A known configured
mode may be shown while worker evidence is missing; loaded metrics stay unknown.
Without a selected demand, outcome is not-evaluated, not a claim that a run fits
or that the work queue is idle. A selected but incomplete/unverified producer
declaration is unavailable/unknown. Raw global resource observations alone cannot
stand in for the complete native-with-build action.

`overrides` contains `status` (`complete`, `incomplete`, `unavailable`), `entries`
and `omitted_count` (nonnegative integer or null when unknown). A complete empty
list means no applicable override; an unavailable/incomplete list does not.
At most 64 entries are projected within the same overall byte bound. Each entry
has `resource`, `domain`, `property`, `value` (N2 quantity) and `source`. Property
is `maximum` or `protected-memory-reserve`; the latter is valid only for memory
and identifies M3's H minimum, not B or the runtime F_mem margin. Maximum
identifies an explicit upper limit for the named resource. Resource is
`memory`, `cpu`, `tasks`, `descriptors`, `native-state-bytes`, `native-sessions`
or `scratch-bytes`. Source is `explicit-argument`, `explicit-environment`,
`recorded-operator` or `recorded-legacy-override`, backed by L1 provenance rather
than guessed from numbers. Scope the inventory to the selected deployment and
inspected action; old unrelated root settings are not that action's overrides.
Per-root limits use response-local `root-N` ordinals, not filesystem ordinals.
Render these as explicit overrides. If legacy intent is unclassified, mark the
inventory incomplete and label that uncertainty; do not pretend the fixed value
is an explicit override or report an empty complete list. No source filenames,
environment variable contents, actor IDs or provenance digests are emitted.
Override units are byte for memory/native-state/scratch bytes, cpu for CPU,
task for host tasks, descriptor for NOFILE, and session for native session count.
Reject a mismatched resource/unit pair instead of formatting it plausibly.
Also reject an invalid property/resource combination. Preserve reserve intent
in L1's configuration/provenance operation, even when it equals the protected
default; do not infer it from a loaded MemoryMax or encode H as a maximum.
Render a protected-memory-reserve as a minimum outside the managed envelope,
distinct from a managed hard limit and from the immediate free-memory margin.

N2. A predicate row contains `kind`, `domain`, `status`, `required`, `available`,
`deficit` and `context`. `status` is `pass`, `blocked`, `unknown`, or `unsupported`.
Unknown numeric evidence is null, never zero. A quantity is
`{numerator: u64, denominator: positive u64, unit: enum}` in reduced form;
unit is `byte`, `task`, `descriptor`, `inode`, `session` or `cpu`. Byte/task/descriptor/inode/session
quantities have denominator 1. CPU quantities retain exact rational rates;
integer-percent display cannot turn a small nonzero detected rate into zero.
Comparisons/arithmetic are checked, without floating-point rounding or saturation.

Predicate kinds are `memory-aggregate`, `memory-host`, `memory-ancestor`,
`cpu-minimum`, `tasks-aggregate`, `tasks-ancestor`, `tasks-host`, `tasks-user`,
`descriptor-process`, `descriptor-host`, `storage-bytes`, `storage-inodes`,
`storage-quota`, `logical-image`, `native-session-count`, `native-retained-bytes`
and `native-stored-bytes`. Native per-root count/byte rows preserve the additional
S4 policy predicates; a free filesystem cannot hide a configured session limit.
Use each contract's actual left-hand required
amount and right-hand available bound: for example memory aggregate is R+Q
versus min(B_loaded,B_current), memory-host is Q+S+F_mem versus MemAvailable,
and memory-ancestor is charged_ancestor+Q+S+F_mem versus its raw limit.
F_mem is M6's runtime memory margin; S4's F is filesystem availability, a
separate byte quantity used only by storage rows. Storage bytes uses H+P+D+Q
versus that filesystem F; storage floors are not the memory protected envelope.
Known deficit is max(0,required-available) in that same unit; a deficit can exceed
new demand when commitments already exceed a limit. Do not subtract headroom
again from an already-net amount just for rendering.

`context` permits only these fixed keys with quantity-or-null values:
`effective_limit`, `protected`, `committed`, `requested`, `desired_limit`,
`published_limit`, `loaded_limit`, `original_peak`, `remaining_peak`, `retained`,
`active_growth`, `process_growth`, `free_margin`, `reservation_slack`.
Each present non-null value uses the row's unit. Absent/not-applicable keys are
omitted; unknown applicable keys are present/null. Thus a CPU scheduling maximum
does not invent a committed CPU reservation, and descriptor host pressure does
not masquerade as an exclusive file-handle pool. Context preserves desired/
published/loaded distinctions and retained-vs-active storage detail through the
actual predicate/domain rows, not a misleading combined RAM/disk scalar.
The retained/active_growth/process_growth keys apply only to storage-byte rows
and correspond to S4's retained durable, other durable growth and process totals.
The free_margin key is a byte quantity on memory host/ancestor rows, exposing
F_mem independently of protected H. Reservation_slack carries M6's upper
per-pass S on those byte rows, and P3's applicable upper per-pass task slack on
task pressure rows. It is never rederived by subtracting independently maximized
displayed commitment/current values. All context values retain their row's unit.

Domains are projection-local ordinals such as `runtime-1`, `storage-1`, `root-1`
and `image-1`, with a fixed `runtime|storage|root|image` prefix and positive decimal suffix.
They identify rows within this response, not stable private root/volume identities.
Internal binding carries full identities only in protected state. Multiple paths
to one filesystem use the same storage ordinal. Do not emit actual paths, mount
names, provider session IDs, process IDs, machine/boot IDs, inode identities,
credentials, environment contents or protected evidence/replay digests here.

The serialized projection is bounded to 65536 bytes and 64 rows. Summarize by
resource kind/domain, prioritizing unknown/unsupported and blocked rows before
passes, in fixed kind/domain order. Record omitted checked rows explicitly;
omission never makes an unknown/blocked full vector available. If even a valid
summary cannot fit, emit a small unknown projection with
`capacity-evidence-unavailable`, preserving the full protected diagnostic. Never
truncate a JSON string into apparently valid evidence or forward arbitrary
source fields/error prose as context.

N3. New projected reason codes are exactly `capacity-memory`, `capacity-cpu`,
`capacity-tasks`, `capacity-descriptors`, `capacity-storage-bytes`,
`capacity-storage-inodes`, `capacity-storage-quota`, `capacity-logical-image`,
`capacity-evidence-unavailable`, `capacity-publication-uncertain`,
`capacity-awaiting-load`, `capacity-protocol-incompatible`,
`capacity-override-conflict`, `capacity-producer-unverified` and
`capacity-native-sessions`. The last identifies the separate configured root/session-count
constraint; it is not mapped to host task exhaustion.
Map protected producer outcomes to these values; unknown input becomes
capacity-evidence-unavailable without copying its text. Configuration uncertainty
and missing evidence remain distinct from a measured zero-capacity deficit.
Overall workflow owner pause, authorization, provider readiness, merge and
deployment facts retain their existing precedence/authority.

N4. Add optional/defaulted `capacity: CapacityStatusV1` to the existing `Progress`
serialization; legacy progress rows without it remain readable and report no
capacity observation. Unknown/malformed future nested capacity data renders as
unavailable while preserving the valid surrounding workflow progress and private
diagnostic, not a fabricated available state or a dropped merge event. Old readers
may ignore the additive field; they cannot certify new capacity from its absence.
Projection/encoding failure does not mutate the R-owned outcome, rewrite a
protected workflow checkpoint or turn completed work into an execution failure.

The R-owned protected workflow capacity observation/checkpoint transaction
supplies this projection for the same workflow binding, demand, selection window
and lease generation. N derives it; it does not maintain a second balance or
query a different ambient policy. Integrate #1914's worker-selected evidence and
#1915's precise-blocker preservation. Only meaningful capacity/configuration
state transitions change the durable notification fingerprint, as in L7.
Raw free-byte/age churn does not create a new Discord update every poll.

An emitted progress event's capacity snapshot is immutable and labelled with its
observation time. An already dispatched outbox row freezes its exact content,
nonce, workflow sequence and destination. Do not refresh that content in place
to hide an uncertain send. A fresh interactive status query may collect newer
read-only evidence; it is distinct from claiming an older event is current.
Capacity recovery is not a model admission, completed stage, autonomous merge,
deployment receipt or a reason to reset their counters.

N5. The future `augmentagent capacity inspect --json` returns this versioned
private diagnostic without creating directories, claims, CI cleanup holds or
changing policy. It uses the actual worker receipt and common read-only inventory
APIs. It must not wrap today's `autopr-health` command as a supposedly pure
collector: that command can invoke pressure pruning even without `--notify`.
Unconfigured or unavailable state returns an unknown diagnostic, not implicit
default limits. Human CLI/private Discord status uses the same typed projection.
An optional authorized workflow target supplies its current protected demand;
the generic inspection without a target returns demand_state=not-selected and
outcome=not-evaluated. It does not claim/advance a queue item to select one.
Interactive autoship status for an existing issue/PR uses that workflow's demand
and binding, rather than silently substituting the cheapest text role.
A completed safe inspection returns command success even for blocked/unknown
outcomes; callers must read the typed outcome, never treat CLI exit success as
run admission. Invocation/authorization failures retain existing nonzero CLI
error conventions and cannot return another owner's diagnostic.

Keep `autopr-health --json`'s existing array-of-findings shape; derive new capacity
findings through its analyzer instead of replacing that array with a new envelope.
Its existing housekeeping authority remains separate from capacity inspection.
Do not feed new private diagnostic contents through the generic webhook/Slack
health router without verified private owner destination. A public-safe status
may expose the allowlisted category and observation availability only.

Private Discord rendering shows mode, current configuration/admission state,
active explicit overrides or their unavailable-provenance label,
the primary limiting resource and its required/available/deficit quantities,
the next observational action/time, and separate delivery state. Preserve the
existing 1900-character message bound and transport nonce marker; summarize
secondary resources rather than clipping the marker or immutable proof content.
The updated human renderer omits the legacy base `evidence_digest` as well as
new protected capacity digests. Preserve the field in legacy serialization and
already-dispatched content for compatibility/reconciliation; do not rewrite history.
Render unknown as unknown, not “0 free.” The existing authorized owner/workflow
destination check applies before any detailed response or update is queued or dispatched.
Its existing ingress/delivery evidence must positively establish the owner-private
destination; a channel name or configured webhook variable is not that proof.
Show the workflow's next action separately from the capacity recheck; a paused
workflow still awaits owner resume even when its resource predicates pass.
No new public-channel fallback or Slack expansion is introduced.

N6. Capacity state and delivery state are independent. Use the existing #1714
outbox and its remote-evidence/explicit-owner-resolution protocol. Later progress
stays pending behind an uncertain send; never skip it or mint another nonce to
claim recovery delivered. The existing successful remote proof or explicit
authorized resolution settles that original uncertainty before later updates.

For an uncertainty episode, status exposes `delivery-uncertain` immediately.
At 10 minutes unresolved age, expose local health/private-CLI attention and
queue one private delivery-attention intent at the next eligible attention slot
through the same service. Attention intents, including a new episode's first
intent, are bounded to one per 6 hours and at most four in the rolling interval
`(now-24h, now]` per owner/workflow/destination across episodes. Persist due times
and deduplication so restart or a new uncertainty episode cannot reset the bound.
Local attention is visible at 10 minutes even while the next intent slot is
rate-limited. Reconciliation runs each ordinary unpaid pass
using #1714's bounded transport operation; insufficient remote evidence stays
uncertain and offers the existing owner-resolution action.

Attention is separate from the capacity/blocker fields and never overwrites
them. An attention intent behind uncertainty may remain pending or coalesce
after successful reconciliation; report it as pending, never delivered. Do not
introduce another sender/route to evade the ordering guard. Local attention
remains visible while the transport cannot deliver. Coalesce only undispatched
reminders/latest status; preserve all dispatched content and evidence. Once
the original delivery is reconciled, ordinary current-state delivery resumes.
Delivery failure never grants/denies execution or changes merge allowance.

### Reporting and delivery fixtures

- Legacy Progress without capacity decodes with no observation; malformed or
  version-2 nested capacity cannot become available or erase a confirmed merge.
- A valid explicit 8 GiB ceiling is labelled as an override even when it equals
  an old default. Unclassified legacy provenance remains incomplete, not an
  inferred override/default. A root's 16-session cap is separate from host PID
  capacity and uses session quantities, even with abundant backing disk space.
- Explicit protected reserve H=8 GiB on a 16 GiB envelope records
  `{resource:memory,property:protected-memory-reserve,value:8 GiB}` and renders
  minimum protected outside capacity; a separate explicit managed maximum
  records property=maximum. Neither becomes an unused-free-memory floor or
  disappears merely because the current computed B is also 8 GiB.
- Required memory 12.5 GiB versus available 12 GiB displays a 0.5 GiB deficit;
  retained disk commitments never render as live RAM use. Detected 1/2 CPU is
  distinct from its rounded 37-percent managed quota.
- Missing worker receipt plus known configured auto mode shows auto intent,
  worker unavailable and unknown loaded values, without the observer's limits.
- Candidate published but worker not reloaded stays awaiting load even if
  numerical predicates otherwise fit. Owner pause still dominates the next
  action; a nested capacity-available result does not resume it.
- Injected paths, credential-like strings, native IDs and unknown reason/context
  keys cannot reach projected JSON/Discord/webhook text. Response aliases share
  one filesystem ordinal; omitted failing rows cannot produce available outcome.
- At t=599 seconds of uncertainty, no attention intent; t=600 queues one, with
  later updates still pending. Restart/repeated passes at t=600 queue no duplicate.
  Another reminder is ineligible before 6 hours; the rolling-day cap still applies.
- A capacity recovery observed while an earlier send is uncertain changes
  current local capacity state but does not claim the recovery was delivered.
  Reconciliation confirms the exact original send before any queued later update.

## 10. Open contract checklist and handoff gate

Memory equations above are proposed engineering decisions with exact fixtures.
They require fresh-reader review and source/integration validation before C0
closure. C1-C5 remain blocked; no source implementation is authorized by treating
this partial document as the completed contract.

- [ ] R/P: full native storage peak, inode/block limits, final SQL/replay/identity
  encoding, and structural all-writer/startup compatibility. Shared accounting,
  lock order and release decisions in section 7 still require integration review.
- [ ] R: complete-vector integration validation of memory/CPU/task/descriptor
  contracts above, including host/guest distinction, shared accounting, finite
  UID task limits and per-process descriptor enforcement.
- [ ] R: storage observation identity, filesystem/inode/quota/image support,
  durable reservation visibility and coherent replacement/recovery rules.
  Memory/CPU observation decisions above still require integration validation.
- [ ] P: auto/fixed config schema, supported legacy/bound migration, managed
  provenance and publication/reload/uncertainty replay states.
- [ ] P/E: snapshot configuration coverage, compatibility/rollback rules and
  preservation of current security, owner controls and accounting.
- [ ] R/H: integration validation of section 8's cadence/hysteresis/shrink/recovery
  decisions, with final stable reason identifiers and worker readback mapping.
- [ ] H/N: source/integration validation of section 9's payload, privacy, outbox
  snapshot/deduplication and bounded-attention contracts, including final persisted
  attention schema and #1714 transport/owner-resolution linkage.
- [ ] E: reviewed complete contract plus fresh-reader, conversation and full seam
  audits; reproducible fixtures covering every contract branch.

This checklist narrows outstanding decisions without moving them into downstream
implementation as unspecified assumptions. Current production policy and owner
holds remain unchanged by this document.
