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
