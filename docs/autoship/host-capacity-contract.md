# Host capacity contract: C0 working decisions

Part of [#1918](https://github.com/nolanmak/Jarvis/issues/1918), under #1917
and #1703. This is an incomplete contract under review, not an implementation
handoff or an assertion about deployed behavior. The
[feature plan](host-capacity-sizing-proposal.md) remains the scope authority.
Decisions below resolve specific questions; the open-contract checklist prevents
those decisions from being mistaken for completion of C0.

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

Let Q be the new run's complete declared memory, R the sum of existing full run
reservations plus non-run charges including M4's daemon minimum, C the observed
non-reclaimable charged memory in that same aggregate, and S = max(0, R - C).
R also includes aggregate kernel charges absent from child accounting, as the
current supervisor does; a child is never counted again as an unrelated sibling.
If hierarchical accounting is inconsistent, return unknown and resample.

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
3. `Q + S + H <= MemAvailable` on the host.
4. For each finite external ancestor, `charged_ancestor + Q + S + H <= limit`.
   Ancestor charges include competing siblings. Subtract its verified reclaimable
   inactive-file cache as in the existing supervisor; unavailable accounting is
   unknown, not an invented zero. For a limit tighter than the plan, recompute
   the smaller envelope before admitting; never reuse a larger H/B pair to
   manufacture a fit.

H is tested against raw host/ancestor headroom, and is already excluded from
B_loaded. Do not subtract H from B_loaded again. These are simultaneous bounds
on the same work, not additive deductions from a net pool. S accounts for idle
runs' future use because `MemAvailable`/ancestor charges already reflect current
usage; adding all R to those usage-based checks would count live usage twice.

For an existing fixed deployment, retain its current supervisor checks until
the supported migration installs the complete auto contract. This document does
not authorize silently replacing its 256 MiB host margin with H.

### Exact admission fixtures

All values below are GiB; B_loaded=12, H=4, Q=6. In the first four rows the
external ancestor is the host and supplies no tighter independent bound.

| R | C | S | MemAvailable | R+Q | Q+S+H | Result |
| --- | --- | --- | --- | --- | --- | --- |
| 0.5 | 0.5 | 0 | 12 | 6.5 | 10 | memory admissible |
| 0.5 | 0.5 | 0 | 9 | 6.5 | 10 | host pressure, deficit 1 GiB |
| 6.5 | 2.5 | 4 | 14 | 12.5 | 14 | aggregate deficit 0.5 GiB |
| 4.5 | 1.5 | 3 | 12 | 10.5 | 13 | host pressure, deficit 1 GiB |

External-ancestor example: E=16, B_loaded=12, H=4, R=C=0.5,
Q=6, MemAvailable=20 on a larger physical host; an external limit of 16
with charged usage 7 requires `7+6+0+4=17`, so refuse with ancestor deficit
1 GiB. Large host availability cannot override that ancestor.

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
- Derived E/H/B and CPU values, existing reservations, proposed complete run
  demand, every evaluated predicate and all limiting deficits. An observation
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
   cpuset and raw ceilings must agree. With charges use the greater observed
   current value and smaller observed reclaimable value; with availability use
   the smaller sample. Clamp derived non-reclaimable charges at zero.
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
  10/9 GiB: use charged memory 3.5 GiB and availability 9 GiB.
- The status collector's cgroup differs from the selected worker: do not use
  local fallback. Return unknown unless it observes the verified target context.
- The boot ID changes, clock goes backward or required controller accounting
  disappears: unknown; previous-boot/timestamp data cannot grant recovery.

O5. Required fields above define the memory/CPU observation contract only.
Storage mount/volume identity, logical image checks, filesystem/inode/quota
support and durable reservation observations must be completed below before C0
closes. A partial memory/CPU result never claims overall capacity is known.

## 5. Native storage producer: verified boundaries

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

## 6. Open contract checklist and handoff gate

Memory equations above are proposed engineering decisions with exact fixtures.
They require fresh-reader review and source/integration validation before C0
closure. C1-C5 remain blocked; no source implementation is authorized by treating
this partial document as the completed contract.

- [ ] R/P: full native storage peak, inode/block limits, shared durable claim
  representation, all-writer compatibility, lock order and crash-safe release.
- [ ] R: PID/FD demand, sizing and admission fixtures; CPU and memory decisions
  above need complete-vector integration validation.
- [ ] R: storage observation identity, filesystem/inode/quota/image support,
  durable reservation visibility and coherent replacement/recovery rules.
  Memory/CPU observation decisions above still require integration validation.
- [ ] P: auto/fixed config schema, supported legacy/bound migration, managed
  provenance and publication/reload/uncertainty replay states.
- [ ] P/E: snapshot configuration coverage, compatibility/rollback rules and
  preservation of current security, owner controls and accounting.
- [ ] R/H: runtime resampling cadence, increase hysteresis, shrink/unknown/recovery
  semantics, stable reason identifiers and worker effective readback.
- [ ] H/N: versioned CLI/private Discord payload mapping, privacy classification,
  deduplication, uncertain delivery reconciliation and bounded alert lifecycle.
- [ ] E: reviewed complete contract plus fresh-reader, conversation and full seam
  audits; reproducible fixtures covering every contract branch.

This checklist narrows outstanding decisions without moving them into downstream
implementation as unspecified assumptions. Current production policy and owner
holds remain unchanged by this document.
