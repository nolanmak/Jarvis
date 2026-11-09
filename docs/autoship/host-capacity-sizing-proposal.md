# Host-aware autoship capacity sizing — proposal

## 1. Status and purpose

Draft under #1703 and resource ownership #1676. This records the owner's request
to optimize capacity for the machine running Jarvis. It is not implementation
handoff, a policy approval, an admission capability, or restoration evidence.
Resolve the load-bearing questions in section 8 before enabling automatic policy
changes. The immediate legacy capacity recovery in #1913 is independent and must
not wait for this project. #1914 reports capacity; #1915 preserves command blockers.

The November 8 incident had eight retained runtimes, each charged a conservative
1 GiB reservation, against an 8 GiB retained-state budget. A new runtime required
9 GiB. Freeing unrelated build caches did not change that policy limit. The
proposed 16 GiB replacement remains an operator choice, not an applied setting.

## 2. Measurements and existing owners

Read-only observations on November 9, 2026 around 01:36 UTC (November 8 in New
York): the state filesystem `/` had about 39 GiB available; host RAM was about
15.5 GiB total and 6 GiB available; nearly all 4 GiB swap was used. The runtime
slice declared MemoryHigh=8 GiB, MemoryMax=10 GiB, CPUQuota=400%, TasksMax=2048.
These are a historical sample, not values to hardcode or current admission proof.
`/mnt/build` and `/mnt/store` are different filesystems. Archive free space is
not hot-state capacity. The sample's slice limit is not proof that no ancestor
has a tighter effective limit.

Reuse these owners rather than introducing another allocator:

- `native_vm.rs`: policy, retained-session demand, state lock, native admission.
- `native_state_capacity.rs`: read-only storage observation and retry fingerprint.
- `run_resources.rs` and the descendant supervisor: execution demand, composed
  native/build guest memory, controller overhead, live reservation admission.
- `build_storage.rs`, `storage_reservations.rs`, `storage_leases.rs`: build growth
  reservations and coordinated cleanup/admission.
- Existing updater/provisioning: policy publication and guarded reload.
- Workflow capacity retry, health and private outbox: waiting, recovery and
  truthful delivery. None is a second admission authority.

## 3. Decisions and reasons

D1. Separate retained-state bytes, retained-session count, and concurrent
execution resources. Increasing a disk budget changes neither guest RAM nor
concurrency. Raising bytes can still leave the session-count limit binding.

D2. Keep every retained runtime's full current reservation until original
resource ownership and checked retirement justify releasing it. Small `du`, old
timestamps, missing PIDs, leases or visible file flags are not future-growth
bounds. This project does not implement reservation discounts or cleanup by age.

D3. Introduce observation/recommendation first. Default behavior is read-only.
An eventual automatic mode requires an explicit owner ceiling and supported
publication integration. Recommendations never rewrite policy by themselves.
The initial sizing target is the smallest retained-byte budget that can cover
the controller-declared next runtime allocation while retaining every existing
reservation. It is not the largest budget the host could support, a forecast of
daily shipments, or a session-count/concurrency tuning algorithm. With no declared
allocation demand, recommend no policy change. Report other binding limits
without recommending changes to them. The demand producer and its revision must
be specified with the observation contract in section 8.

D4. Compute storage headroom on the actual state filesystem, preserving its
configured free-space floor and other writers' outstanding growth commitments.
Do not pool partitions or double-count promises already represented by available
space. There is no approved numerical sizing formula yet: section 8 owns the
materialized-versus-outstanding accounting question. Never infer unused
reservation credit from file-size observation.

D5. Execution admission continues to use the existing supervisor's authoritative
live reservations and effective ancestor limits. Host RAM total, available RAM,
swap size, CPU count, or historical RSS alone cannot authorize concurrency or
shrink the composed native/build/controller demand. A planner may recommend a
limit but every actual launch still passes existing admission.

D6. Stale, missing, unsupported or inconsistent measurements produce unknown
recommendations. They never raise a budget. Linux/cgroup support must be explicit;
other platforms retain their existing fixed policy until separately supported.
The observation contract must identify the source mount and effective resource
scope, observation time, policy/configuration revision, demand and limiting reason.
Private paths and runtime identities stay in protected state; public output is
numeric/allowlisted. A cached observation is never a launch grant.

D7. R1 reports pressure/unknown/recovery without changing admission. Existing
fixed-policy guards continue to decide all launches. R2 must add its pressure
handling inside the existing native allocation owner, not in the reporting
planner: known insufficient headroom denies new allocations; unknown or stale
evidence also denies new allocations while automatic mode is enabled; recovered
evidence permits revalidation through ordinary admission, never direct dispatch.
Enabling automatic mode on an unsupported platform must be refused; fixed mode
retains its existing behavior. The concrete producer/API remains section 8 work.
Do not revoke a live guarantee, kill retained sessions,
silently lower a configured budget below existing commitments, override floors,
or rewrite owner holds. Once a supported observation shows recovery, existing
fingerprint reconciliation may wake a queue; ordinary admission runs again.
Reject a requested owner ceiling below current retained commitments and leave
the existing configuration unchanged, showing required versus requested bytes.
If commitments later exceed an already configured ceiling, retain those
commitments, deny additional allocations and report the deficit until checked
retirement reduces demand or an approved ceiling change resolves it.

D8. Automatic publication, if enabled later, must reuse reviewed policy/schema
support, source/candidate revisions, original coordination locks, atomic durable
publication and explicit uncertain-outcome recovery. A newly computed candidate
must not replace a possibly committed candidate after a lost acknowledgement.
The legacy command in #1913 does not support modern bound-owner policy migration;
unsupported schemas remain blocked until their own migration protocol exists.

## 4. Proposed delivery sequence and testable exits

R1 — Read-only sizing report in existing capacity diagnostics. Resource owner R
implements it; H/N consume the same result. First add failing fixtures for:
8 retained + 1 new runtime at an 8 GiB cap; bytes available but session cap full;
large archive with constrained state mount; shared-filesystem writers; smaller
ancestor cgroup; unlimited leaf with finite ancestor; unavailable/stale samples;
and integer overflow. Exit: useful recommendation or explicit unknown without
policy, session, reservation, model-call or allowance mutation. Implementation is
blocked on section 8's observation and accounting contracts.

R2 — Opt-in bounded sizing in existing admission/publication owners, after R1's
contract is resolved. Add failing fixtures for competing publishers, changed
policy/mount/reservations between plan and apply, mount loss, pressure after
publication, source replacement, lost rename acknowledgement, failed sync,
restart/replay, unsupported schemas and a lower owner ceiling than live demand.
Exit: supported policy changes retain all accounting commitments and ordinary per-launch
admission remains mandatory; uncertainty is durable and visible. A source-hash
check alone does not validate resource headroom. Until the common reservation
transaction is designed and implemented, automatic increases remain disabled.

R3 — Current-host rollout and portability acceptance. Use normal reviewed CI,
deploy snapshot and updater guards. Observe an authorized queued workflow reach
actual model work after capacity recovers, then verify required checks and merge
receipts separately. Exercise shrink/recovery on disposable hosts before enabling
automatic mode in production. No production pressure injection or privileged
filesystem transition is authorized by this proposal. This is recovery acceptance;
the full CLI/Discord, five-shipment and soak requirements of #1703 remain open.

## 5. Non-goals

No new queue, allocator or workflow engine; no self-hosted public-PR runner;
no archive migration; no automatic mount/provisioning changes; no session
reservation discounts; no raising a resource ceiling merely to meet the daily
shipping target; no benchmark restart. Independent review, hosted CI, owner
authorization and existing holds stay required.

## 6. Glossary

Retained-state budget: policy ceiling for all retained runtime disk reservations.
Execution reservation: live descendant resource commitment for a running attempt.
Available bytes: filesystem measurement, not unspent native-policy allowance.
Recommendation: observational data that cannot authorize a launch or policy write.
Owner ceiling: explicit upper bound for an opted-in sizing policy, not a promise
that that amount is physically available. Retirement: positively checked release
of original runtime ownership, not absence from a process list.

## 7. Deferrals and risks

Actual use may remain far below conservative reservations. Carrying cost: more
waiting and a larger disk budget until original-resource closure/retirement is
available. R owns that separate work; its trigger is positive custody/resource
proof, not this host's current free-space measurement. Do not backfill that proof.

Other applications can consume free space after any observation. Existing
per-launch and available in-run resource enforcement remain mandatory; this does
not claim that current native retained-state accounting enforces a kernel disk
growth bound. Automatic sizing cannot promise global physical capacity from a
sample. Section 8 must document which shared writers participate, which physical
resources are actually bounded, and how uncoordinated exhaustion is surfaced and
recovered. Retaining accounting commitments is not a physical space guarantee.

## 8. Open threads — block implementation handoff

1. R: specify one cross-writer, per-filesystem growth-accounting contract and its
   real producer/consumer APIs. Separate materialized usage from outstanding
   commitments without double credit. Include native writes, builds, snapshots
   and unrelated consumers. Blocks recommendation formula and automatic writes.
2. R/P: specify measurement identity, freshness, ancestor-limit resolution,
   source consistency and under-lock revalidation boundaries. Include filesystem
   replacement/remount and concurrent reservation/publication. Blocks R1/R2.
3. Owner/R: select explicit ceilings and opted-in resources. The proposed 16 GiB
   immediate recovery value is not that general policy approval. Blocks automatic
   mode only; diagnostics and existing fixed-policy recovery can proceed.
4. R: define supported legacy versus bound-owner publication/reload behavior,
   concurrency control, uncertainty persistence and recovery before R2. Existing
   bindings must never acquire new authority from a planner's recommendation.
5. H/N/E: define bounded private reason codes, freshness display, alert deduplication
   and recovery evidence once R's contract settles. Blocks reporting handoff and
   R3 acceptance, not the independent #1914/#1915 fixes.
6. R: verify actual disk-growth enforcement coverage and define residual
   exhaustion/recovery behavior for uncoordinated consumers. Blocks safety claims
   and R2 activation; neither a planner nor current reserved-byte accounting is a
   substitute for that evidence.

This proposal deliberately exposes those missing contracts. No implementation
issue should claim automatic sizing is ready until they have concrete answers.

## 9. Proposal checkpoint audit

A fresh reader raised eight findings. Four exposed missing decisions now recorded:
the next-allocation retained-byte target, phase-specific advisory versus admission
behavior, ceilings below existing commitments, and the distinction between
accounting reservations and physical disk-growth enforcement. The remaining
accounting, observation, publication/reload and reporting contracts are explicitly
parked in section 8 with owners and blocked phases. This checkpoint is not a
handoff audit; a full seam audit is still required after those contracts settle.
