# Automatic capacity: planning audit record

Scope: publication of the C0 design task and C1-C5 blocked implementation tasks.
This is not implementation acceptance, GitHub code-review approval, a resource
grant or deployed restoration. C0 still owns exact equations, growth/lifetime,
observation, configuration/publication and reporting contracts.

Epic: [#1917](https://github.com/nolanmak/Jarvis/issues/1917). Contract task: [#1918](https://github.com/nolanmak/Jarvis/issues/1918). Documentation PR: [#1916](https://github.com/nolanmak/Jarvis/pull/1916).

Fresh-reader review plus 56 mapped seam checks across 19 focused review groups were completed. The author also checked conversation requirements against the document and verified source-backed findings. Targeted rechecks covered the corrected findings.

## Resolutions and tracked blockers

- S10: raw effective limits and net usable capacity are distinct; headroom is subtracted once (D2/glossary). Exact equations remain C0 work.
- S32/S34: N owns versioned resource/deficit payloads and uncertain-send reconciliation, including pending updates, visible uncertainty and bounded escalation (D9/C0). Detailed contracts block implementation.
- S15/S16/S40: retained commitments survive daemon/process exit; process-flock reclamation cannot release them (D4/D5). Positive retirement, native-growth producer and physical-enforcement limits remain explicit C0 blockers.
- S30: migration needs capacity/service configuration coverage in the existing retention-managed snapshot protocol, plus compatible rollback/roll-forward preserving security revocations/accounting/owner controls (D8/C3). Existing database/binary snapshots alone do not establish this coverage.
- S53: managed outputs include the aggregate runtime slice and service limits, with provenance. Planning excludes verified managed outputs; actual admission includes every enforced limit until reload/readback (D8).
- S33: public/private surface classification and field redaction remain C0-owned before reporting implementation.
- Fresh-reader findings on platform support, legacy override provenance and managed-limit bootstrap order are explicit in sections 1/D8/C0. Exact sizing and native growth remain design-task outputs, not decisions for downstream agents to invent.

## Seam coverage

| Mapped seams | Area checked | Result |
| --- | --- | --- |
| S1, S3, S7 | outer authority link; outer retained-runtime recovery link; implementation/deployment status terminology | Holds at planning scope |
| S2, S13, S20 | outer storage-ownership link; admission-owner handoff; native/shared-storage integration handoff | Holds at planning scope |
| S4, S28, S45 | amendment and product-mode decision; deployment/migration lifecycle; specification audit and publication handoff | Holds at planning scope |
| S5, S21, S48 | legacy recovery boundary and outer link; fixed overrides and rejected configuration; crash/uncertainty protocol across producers | Holds at planning scope |
| S6, S32, S34 | reporting integration and outer links; blocker/status payload handoff; private delivery lifecycle | Resolved or explicitly C0-blocked; rechecked |
| S8, S11, S39 | resource-dimension vocabulary; declared per-run demand handoff; run-resource/supervisor outer links | Holds at planning scope |
| S10, S14, S19 | protected headroom and usable-envelope decision; commitment vocabulary and preservation; full-growth-claim accounting contract | Resolved or explicitly C0-blocked; rechecked |
| S12, S24, S25 | observation versus execution authority; observation identity, freshness and coherence; unknown-evidence behavior | Holds at planning scope |
| S15, S16, S40 | release-proof handoff; accounting versus physical-growth guarantee; storage API side effects and lease lifetime | Resolved or explicitly C0-blocked; rechecked |
| S17, S18, S41 | filesystem identity and device accounting; duplicate-path claim accounting; build/snapshot consumer outer links | Holds at planning scope |
| S22, S23, S35 | shrink/pressure handling; recovery-to-scheduling handoff; fairness, holds and capacity deferral | Holds at planning scope |
| S26, S27, S44 | lock/revalidation protocol; cadence, hysteresis and event deduplication; task dependency and design-readiness gate | Holds at planning scope |
| S29, S30, S38 | durable publication and reload handoff; rollout/storage-control handoff; native-admission outer links | Resolved or explicitly C0-blocked; rechecked |
| S31, S33, S49 | current-worker effective readback; public/private information boundary; successful-shipment acceptance handoff | Privacy contract explicitly C0-blocked |
| S36, S42, S46 | fixed-default initializer outer links; incident-to-contract evidence boundary; test-evidence contract | Holds at planning scope |
| S43, S47, S50 | C0 ownership and consumer-contract handoff; managed-build outer link; non-goal scope boundary | Holds at planning scope |
| S9, S37, S53 | effective-limit computation; service-resource initializer outer link; managed leaf outputs versus enclosing-limit inputs | Resolved or explicitly C0-blocked; rechecked |
| S51, S52, S54 | supported-platform/profile boundary; unsupported activation versus temporarily unknown observation; managed-limit provenance and bootstrap/readback sequence | Holds at planning scope |
| S55, S56 | explicit scoped auto-migration operation; product authorization versus authenticated publication operation | Holds at planning scope |

Specification SHA-256: `37827a3c6b764c27687276f0e451290e2b33b4f31c4cf184d4af8cc42800e072`.

The detailed mapping and focused review notes were retained as session audit artifacts; this repository record preserves coverage and dispositions. Later contract or implementation changes require their own appropriate review and acceptance.

## C0 working checkpoint after issue publication

The separate [working contract](host-capacity-contract.md) now proposes exact
memory planning/admission equations and nine worked fixtures, and records the
source-verified native storage producer inventory. The planning audit above does
not approve that new contract or close #1918.

A fresh-reader checkpoint found one wrong-build ambiguity: Q could have been
charged against a daemon-only service ceiling for a worker launch. Section 2
now restricts that check to the new run's actual enforced ancestor chain, with
cross-service accounting at the real shared aggregate. Seven remaining findings
correspond to the contract's explicit open gates: storage, observation, other
resource dimensions, migration, runtime adaptation, rollback and reporting.
They remain owned by C0 and block downstream implementation. The reviewer found
the memory arithmetic internally consistent; the author independently recomputed
all nine numerical fixtures. This is arithmetic/design evidence, not runtime
test evidence. Full seam review is still required before implementation handoff.

The next C0 checkpoint specifies exact fractional CPU conversion, per-run
maximum versus reserved memory, desired/enforced reload boundaries, and a
memory/CPU observation protocol with conservative two-pass sampling, a 250 ms
collection bound, 500 ms freshness and two-attempt limit. Five CPU planning
fixtures, two reload-boundary cases and the observation boundary arithmetic
were recomputed. A fresh-reader recheck found no new wrong-build/forced-guess
findings in these sections. Shared supervisor scope, managed provenance and
real-entry-point replacement/shrink/cleanup evidence remain implementation
validation requirements; PID/FD and storage observations remain open C0 work.
This checkpoint is not the complete-contract seam audit or C0 closure.

The host task/descriptor checkpoint adds complete role declarations, full host
task reservations, effective cgroup/global/real-UID constraints, per-process
descriptor enforcement and a separately labelled system file-table pressure
check. Guest task/descriptor capabilities are not host reservations. The first
fresh read found two forced guesses: NPROC's process-local soft threshold versus
real-UID accounting, and the enabled role universe behind the task/memory ratio.
Both are resolved explicitly in P2/P3 and rechecked. The descriptor contract also
requires exact child limit/privilege readback and rejects inherited descriptors
outside that ceiling. No new wrong-build/forced-guess findings remained in the
recheck. Native-only role sizing, process-limit and pressure-boundary fixtures
were independently recomputed; this remains design evidence, not runtime tests.

Read-only deployed unit inspection at this checkpoint reported daemon/worker
task use 13/5, each TasksMax=2048, NOFILE soft/hard=4096 and NPROC soft/hard=52942.
It confirms the current separate service limits, not the proposed common
reservation accounting or its implementation. Nothing was published/reloaded.

The shared-storage checkpoint specifies one common admission owner, durable
commitments in each existing native database, a durable per-UID root registry,
per-filesystem full-demand arithmetic, pending/held/release-pending/released
lifetime, original-proof release ordering and create/resume/build/snapshot
fixtures. It includes the Python bridge's sparse-cache/command-storage consumers,
which do not currently publish Rust shared claims. The native build cache's
12 GiB demand and 20 GiB scratch floor remain distinct from retained 1 GiB
session accounting; auto mode must replace its managed 24 GiB aggregate default
without discarding explicit operator ceilings.

A fresh-reader checkpoint found the proposed arithmetic and lifetime handoffs
consistent. A continuation-pressure question led the author to require a
verified remaining-growth bound U, defaulting to the full original component
when unproven. Targeted recheck confirmed the corrected fixtures and that only
the qualified original writer may use U; every other writer counts the full
durable commitment. The author recomputed snapshot integer-byte rounding,
multi-root/device, concurrent-admission and continuation examples independently.

Final producer bounds, filesystem/identity encoding, SQL/replay/bridge APIs and
structural mixed-version/startup fencing remain C0 gates. This checkpoint is
not complete implementation readiness or a full-contract seam audit. The
separate executable/companion bound correction is implemented in PR #1925,
with local behavioral red/green evidence; it does not close these wider gates.

The lifecycle checkpoint separates configured mode, desired output, durable
publication, actual loaded enforcement and current admission. It defines uncertain
publication replay, resource-projection rollback preserving current authority,
and runtime adaptation using the existing unpaid reconciliation loop. A managed
increase needs two matching known observations at least 60 seconds apart; real
admission uses fresh locked evidence, shrink blocks new work immediately, and
ordinary contention recovery does not wait for another increase-stability cycle.
Unchanged resource deficits do not reopen productive hourly attempts.

The author checked these decisions against the updater's existing guarded
policy-only activation, worker upgrade watch and capped 300-second independent
reconciliation loop. Fresh-reader review found no new wrong-build/forced-guess
findings in L1-L7 and confirmed the timing/accounting examples. These are working
design fixtures, not implemented fake-clock/runtime test results. Final schemas,
step journal, compatibility/projection matrix and structural startup fence
remain load-bearing C0 gates before downstream handoff.

The reporting checkpoint defines CapacityStatusV1, typed predicate quantities,
explicit demand selection, bounded safe override provenance, legacy Progress
compatibility and unchanged health-array output. The author verified the current
Progress/outbox serialization, 1900-character message guard and health collector's
pruning side effect. Pure capacity inspection therefore uses a separate read-only
path; worker unpaid original-proof reconciliation remains allowed to clear
recoverable holds without waiting for a productive attempt.

Fresh-reader review found a real visibility gap: the first projection could not
label an active explicit override. N1/N5 now carry/render a bounded typed override
inventory, evidence completeness and safe provenance. Recheck found that resolved,
including root/session versus filesystem/PID distinctions. Additional author
checks preserved immutable dispatched content, separate workflow/capacity actions,
privacy redaction and attention limits across restart and repeated episodes.

The delivery checkpoint keeps later updates behind uncertainty, exposes local
attention after 10 minutes, and bounds private attention intents to one per six
hours/four per rolling day across episodes. Pending attention is not claimed
delivered and never determines execution or allowance. Final persisted attention
schema and #1714 integration remain explicit C0 gates. These are proposed
behavioral fixtures and reviewed working contracts, not implementation/live
acceptance or the final whole-contract seam audit.

The compatibility checkpoint adds S8/S9's proposed monotonic protocol floor,
same-inode negative open fences, staged old-writer handoff, and boot/rollback
requirements. In particular, a writer's already-open descriptor survives chmod;
new-client rejection cannot stand in for checked old-controller handoff. The
normal native-owner history contributes correlation, not cleanup authority.

The disposable Linux probe in `probes/storage_compat_permissions.py` passed
22 predicates across storage-temp and private systemd-user runtime locations:
12 permission/flock predicates and 10 actual legacy Python scratch-gate
predicates. It records the imported bridge SHA-256; at this checkpoint that is
`42bbc1aa57cbc2e750f9ee804132aee0799adedeb0233857974676b39e390c6f`.
It creates only private temporary files and never creates a VM image, opens an
application state/lock, invokes a model, or changes production permissions.
The mode-01700 scratch-root discriminator rejects the legacy allocator's exact
0700 validator, while a preopened root descriptor remains usable. This is
evidence for that negative gate, not a complete migration test or a grant.

Additional source tracing includes the Rust compute-retention `usage.json`
publisher consumed by Python scratch admission. It must be covered by the final
consumer/startup matrix. Scratch-root mode alone does not fence that allocator
or every sweeper. New protocol session paths avoid the legacy immediate-child
session-prefix sweep; original live legacy paths must not be moved.

The compatibility audit identified eight already-open C0 implementation-contract
gates: original handoff producer/completeness, full bootstrap entry-point matrix,
protocol/marker/registry formats, per-step journal/crash recovery, snapshot
projection/compatibility matrix, actual growth/inode/quota profiles, held bridge
APIs, and exact SQL/config/provenance/legacy-versus-bound migration handlers.
These remain prerequisites for implementation handoff, not follow-up cleanup
after activation. The checkpoint does not close C0 or unblock C1-C5.

A fresh-reader recheck found a boot-specific ambiguity: fence recreation could
be read as enough to reopen admission after an interrupted cutover. S9 now
explicitly resumes the same journal and requires checked handoff/import/
publication/loaded completion before admission. The probe now refuses an
unexpected legacy-source SHA and executes the exact hashed bytes, and the
contract explicitly limits temporary/runtime observations instead of presenting
them as certification of production filesystem profiles. Final pinned legacy
entry-point and full migration/profile tests remain required.

The quota-observation checkpoint (O7) distinguishes read-only Q_GETFMT success,
ESRCH inactivity and unknown errors, with both-pass coherence and complete-vector
requirements. Source verification against the Linux quota API/kernel showed
Q_GETFMT does not require special quota privilege. The non-root x86-64 probe
returned ESRCH on all 12 user/group/project queries across the current state,
build, store and archive devices. Separate mount inspection identified ext4;
the command/input order and per-descriptor device IDs preserve traceability.
No quota, policy, accounting or production file was changed. These are current
CLI samples, not worker grants or a complete storage observation implementation.

Fresh-reader review found no new material contradiction in the checkpoint.
Two evidence-polish points were resolved: the probe says non-root rather than
claiming full absence of capabilities, and records device IDs alongside the
documented input order. Exact active-quota profiles, protected typed encoding,
filesystem identity/support, image/inode/durable balances and complete collector/
admission integration remain explicit C0 gates. An inactive quota result cannot
be used to manufacture numeric unlimited capacity or ignore another predicate.

The filesystem checkpoint (O8) separates persisted full ext4 UUID identity from
current device/mount/namespace/object attachment. It specifies held-path
revalidation, single-pool alias arithmetic, collision handling, reboot accounting
rebinding and descriptor-based available-byte/inode counter units. A UUID match
never supplies source provenance, native custody, execution or cleanup authority.
The inspector remains separate from admission's mutating CI-hold reconciliation.

The non-root filesystem probe read nonzero 16-byte UUIDs and held-descriptor
counters on the four current devices without exposing UUIDs, reading raw block
devices or invoking SET/mount operations. Three disposable predicates confirm
held-directory stability after replacement, reopened replacement inequality and
same-filesystem identity despite distinct directory inodes. Pairwise comparisons
also distinguish the four input devices without collision and recognize a
repeated repository input. This is not bind-mount, actual worker-namespace,
statx-attachment, publication-boundary, reboot or complete migration acceptance.

Fresh-reader review found no new hard contradiction. It confirmed that complete
global inventory revision semantics, descriptor-bearing constructor interfaces,
root/database/anchor encoding, write profiles and the rest of storage admission
remain load-bearing C0 gates. Its experiment-polish suggestion to emit pairwise
input comparisons was implemented; these comparisons do not enlarge the stated
evidence scope. No production configuration/accounting was modified, and C1-C5
remain blocked pending complete C0 closure and the final whole-contract audit.

The bounded memory component in `host-memory-facts-contract.md` received a
fresh-reader audit, author conversation/source cross-check and all 16 mapped
seam checks (six grouped read-only auditors). No retained component-contract
finding remained. This closes that small implementation contract, not full C0.
The feature plan explicitly permits these settled data helpers and strengthening
the existing supervisor decoder under C1 while complete discovery, accounting,
migration, activation and remaining C0 work retain their prerequisites. This
removes an overbroad helper dependency without weakening any execution gate.

The existing `available_memory()` source accepts the second token on the first
matching line without validating total, units, duplicates or u64 range. TDD
fixtures in the implementation worktree reproduce 11 malformed observations
accepted through that unchanged production entry on main `416f392b`; valid
kB/zero and missing-availability compatibility cases run separately. Missing
new parser/planner APIs are not counted as deployed behavioral regressions.
Implementation/CI/independent review/rollout evidence remains separate.

The memory-admission refinement separates the protected managed-envelope share
H from the existing runtime free-memory margin F_mem=256 MiB. H still reduces
the managed maximum once; current OS/unrelated use is accounted by MemAvailable
and ancestor charges. The revised busy-host, aggregate, external-pressure,
explicit-reserve, equality and one-byte-deficit fixtures were recomputed with
exact rational/integer arithmetic. The proposal adds the runtime margin to its
external-ancestor predicate; it does not change the deployed fixed-mode policy
or the closed M1-M3 helper component in PR #1929.

Fresh-reader plus author source/conversation checks and 24 mapped amendment
seams were reviewed in eight groups. Two substantive findings were resolved:

- Maximizing aggregate charged memory and then subtracting it from reservations
  can minimize future slack and falsely fit a new run. M6/O3 now derive R_i,
  C_i and S_i in each complete pass and retain upper R and upper S separately.
  The decreasing-charge fixture exposes the formerly false 8.25 GiB fit and
  requires 10.25 GiB against 9 GiB available. Root-added seam 25 covers the same
  task-accounting correction in P3/O6 and its typed N2 projection: the task
  counterexample correctly refuses 2008 required against 2000.
- A protected-reserve minimum could not be represented by N1's old ceiling-only
  entry. L1/N1 now retain typed property/value/provenance, distinguish maximum
  from protected-memory-reserve, and render the latter as an outside-capacity
  minimum. Equal numeric H/B values do not merge these meanings.

Rechecks found both findings resolved, including the additional task seam and
fresh-reader equality-boundary suggestion. All 25 scoped seams hold at this
checkpoint. This is not a full working-contract handoff audit, code approval or
current-worker admission evidence. Existing complete-vector, common accounting,
observation/provenance, SQL/migration/rollback, original handoff and projection
integration gates remain open. No production setting, reservation, retained
session, model call or shipping allowance was changed by these design checks.
