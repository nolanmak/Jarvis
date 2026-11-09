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

O6. Section 5 extends the same identity/clock/read protocol with typed per-scope
pids.max/current, host task counts and kernel ceilings, the actual launching
process's soft/hard NPROC and NOFILE, real/effective UID and namespace identity,
same-real-UID task counts, file-table counters, nr_open, inherited descriptor
indices, privilege-enforcement readback and the enabled complete role declarations.
Valid zero ceilings are known exhausted limits; they are not missing data.
Unlimited NPROC is distinct from an unavailable threshold. Same-UID usage is
not a substitute for identifying the launcher's own enforced process limit.
Limits/declarations/identities must agree across passes; use greater task/file
usage and smaller availability. Newly applied child limits/privilege state are
verified at the original pre-release boundary rather than trusted from a saved
JSON report. No overall known result is claimed before storage evidence exists.

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
chain of the new run, let R_tasks be existing full run declarations plus
max(64, non-run task use), C_tasks the scope's current observed tasks and
S_tasks=max(0,R_tasks-C_tasks). Admit only if:

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

S8. Mixed-version/startup fencing is still load-bearing C0 work. In particular,
old v1 readers delete unlocked process claims before parsing their version, and
old native admission never consults the shared registry. A new table/row type
or a documented “upgrade every writer” convention does not structurally fence
them. Before auto activation, the supported migration must verify the complete
managed writer set, enforce a compatible minimum version/startup barrier across
reboot and rollback, and positively settle incompatible active writers. Unknown
compatibility keeps activation blocked. This section specifies durable balance
and ordering; it does not yet approve that compatibility protocol or bootstrap
automatic mode on an existing legacy host.

## 8. Open contract checklist and handoff gate

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
- [ ] R/H: runtime resampling cadence, increase hysteresis, shrink/unknown/recovery
  semantics, stable reason identifiers and worker effective readback.
- [ ] H/N: versioned CLI/private Discord payload mapping, privacy classification,
  deduplication, uncertain delivery reconciliation and bounded alert lifecycle.
- [ ] E: reviewed complete contract plus fresh-reader, conversation and full seam
  audits; reproducible fixtures covering every contract branch.

This checklist narrows outstanding decisions without moving them into downstream
implementation as unspecified assumptions. Current production policy and owner
holds remain unchanged by this document.
