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

## 3. Native storage producer: verified boundaries

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

## 4. Open contract checklist and handoff gate

Memory equations above are proposed engineering decisions with exact fixtures.
They require fresh-reader review and source/integration validation before C0
closure. C1-C5 remain blocked; no source implementation is authorized by treating
this partial document as the completed contract.

- [ ] R/P: full native storage peak, inode/block limits, shared durable claim
  representation, all-writer compatibility, lock order and crash-safe release.
- [ ] R: CPU quota/affinity fractions, minimum versus maximum CPU/PID/FD demand,
  scheduling/fairness and their exact fixture outputs.
- [ ] R: observation schema, scope identity, monotonic freshness, coherent read
  protocol and allocation-lock revalidation/replacement rules.
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
