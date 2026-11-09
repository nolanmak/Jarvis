# Automatic host-sized Autoship capacity

## 1. Outcome, status and authority

Under #1703, with existing storage ownership in #1676 and retained-runtime
recovery in #1902. On a supported fresh deployment Jarvis must detect the
machine's effective resources and configure usable Autoship capacity automatically.
The owner must not have to discover and replace an arbitrary 8 GiB default.
Revalidate admission as resources and competing work change. Explicit operator
ceilings remain overrides; unavailable evidence must never mean unlimited.

Initial support is the existing Linux Autoship deployment with cgroup v2 and
the existing delegated runtime/supervisor controls. This does not expand macOS
deployment scope. Unsupported platforms/controller arrangements refuse auto
activation explicitly; supported hosts with temporarily unavailable measurements
retain configured auto mode with admission blocked and a recoverable reason.
C0 must enumerate filesystem/profile support and these distinction tests.

This is the feature plan, not a claim of implementation or deployment. The first
contract task below must close the listed engineering decisions before dependent
coding tasks start. Automatic deployment is the product requirement; the former
read-only-default proposal is superseded. An inspection command remains read-only.
The owner has requested automatic sizing on the current host; another choice of
an arbitrary fixed capacity is not a prerequisite to designing this feature.

Dependency refinement: the closed
[host memory facts/default-sizing component](host-memory-facts-contract.md)
may be implemented under C1 while the remaining C0 contract stays open. It
strengthens the existing supervisor decoder and supplies M1-M3 data helpers;
it adds no activation, configuration writer, full-capacity status or new
admission authority. Complete C1 and dependent integration/activation remain
blocked on their full reviewed prerequisites. Do not treat helper completion
as complete detection, automatic sizing or deployed recovery.

#1913 is the separate guarded legacy-budget recovery command, #1914 reports
capacity and #1915 preserves precise private blockers. They do not implement this
feature. Their reviews/rollouts and the existing restoration work continue; the
capacity feature must not be misrepresented as already live.

## 2. Resource model and decisions

D1. Capacity has separate dimensions: retained runtime disk state, concurrent
execution memory/CPU/PIDs/file descriptors, and build/snapshot growth on each
actual filesystem. Eight retained runtimes do not mean eight GiB of live RAM.
Memory totals cannot size a disk budget. Archive free space cannot fund a state
or build filesystem on another device. Logical VM filesystems need their own
usable-space checks in addition to backing storage.

D2. Fresh supported deployments default to automatic sizing. Detect host memory,
currently available memory, CPU affinity and effective ancestor cgroup limits,
current commitments, and the actual state/build/snapshot filesystem identities.
Compute an aggregate usable envelope after protected system/daemon headroom and
existing commitments. Fit complete declared runs within it; do not shrink a
native guest, build guest or controller's required demand to make admission pass.
A machine too small for one required run reports that precise blocker.
Swap is a pressure signal, not additional physical RAM for admitting guests.
C0 must distinguish hard minimum demand from maxima and schedulable shares for
each dimension; a fractional CPU limit must not be mistaken for zero CPUs.
Raw effective limits and the usable envelope are separate quantities: subtract
protected headroom exactly once in the envelope, not again from an already net
limit. C0 must make that convention explicit in fields and worked fixtures.

D3. Use one admission authority for each resource through the existing owners.
Reuse native admission, the descendant supervisor, storage reservations/leases,
provisioning/updater, health and the private outbox. Detection/planning returns
data, never an execution grant. Reserve actual commitments on demand; do not
reserve every resource's entire theoretical capacity for one subsystem and starve
builds or other eligible work. Scheduling fairness and owner holds remain intact.

D4. Preserve full current retained-session demand until positively checked
retirement releases it. File sizes, age, a missing PID and absence of a visible
writer do not justify discounts or deletion. Current native accounting charges
1 GiB per retained session; that policy reservation is not a demonstrated kernel
bound on all future disk growth. Keep accounting and physical guarantees distinct.
Uncoordinated applications can still consume physical capacity; define truthful
pressure/failure recovery rather than promise exclusive space from a sample.

D5. Storage accounting is per actual filesystem device and uses conservative
full active growth claims. A claim touching multiple paths on the same device is
counted once. Do not deduct its materialized usage from its remaining promise
without a real producer contract. Native retained demand must join the existing
build/snapshot accounting before automatic allocation relies on a combined view.
No second independent native ledger or allocator may decide the same commitment.
Retained-native commitments outlive a process or daemon restart. Reclaiming a
process-scoped flock claim cannot release those commitments; C0 must specify
their durable integration and original checked-retirement release ordering.

D6. Explicit fixed overrides are supported and visibly labelled. They cannot
exceed effective enclosing hard limits or silently violate protected floors.
Reject a new ceiling below live commitments without rewriting configuration.
If the system shrinks below existing commitments, retain their accounting,
pause new admission and report the deficit. Do not kill sessions, prune unknown
state, reset allowance, clear holds, or revoke a live guarantee to fit a new size.
Recovery revalidates ordinary admission; it does not dispatch work directly.

D7. Measurements bind policy revision, process/resource scope, mount identity,
clock/freshness and demand revision. Missing, stale, replaced, unsupported or
inconsistent inputs remain unknown and block increases/new automatic admission.
The contract task must define coherent sampling, exact freshness rules and
revalidation under the existing allocation locks. Polling is observational; it
cannot bypass the per-launch guards. Define bounded hysteresis for increases and
immediate handling of real admission pressure, without treating every byte change
as useful work or a new alert.

D8. Fresh installation, upgrade of an existing deployment, and runtime adaptation
must all be designed. Preserve explicit legacy overrides unless authorized to
switch them to auto; never infer intent merely because a value equals an old
default. The current owner's request authorizes designing/enabling supported auto
mode, not bypassing code review or migration guards. Use normal deployment
snapshots/updater and effective configuration readback. Bound-owner schemas need
their actual migration protocol; #1913's legacy writer cannot migrate them.
Durable uncertain publication retains the original candidate until reconciled;
never recompute a second candidate to hide a lost acknowledgement.

Installer-managed limit scopes are outputs, including both service limits and
the aggregate runtime slice. Derive their desired values from host/enclosing
constraints outside the verified managed scopes. Preserve external ancestors and
explicit operator ceilings as constraints; names or old numeric defaults do not
prove management provenance. C0 must enumerate the managed scope set, consumers,
provenance and ordering for each supported installation path.
Record managed-limit provenance and verify effective limits after installation
and reload. A legacy 10G service or managed aggregate slice cannot perpetuate itself merely
by being read as the host envelope. Ambiguous legacy provenance stays fixed until a supported
explicit `enable auto` migration records its scope/revision and replay identity.
The current host's owner request supplies the product authorization; the normal
authenticated operation and its durable publication record still provide the
implementation boundary. No new request for an arbitrary byte value is needed.
Excluding managed output scopes applies only to planning their future configuration.
Actual launch admission includes every currently enforced kernel limit, including
managed service and aggregate scopes, until publication/reload and effective
readback prove the complete change.
Report desired, published and observed effective limits separately.

Existing deploy snapshots cover database/binary artifacts, not the new capacity
policy and service migration by assumption. C0/C3 must extend the existing
retention-managed protocol with compatible capacity/configuration revision
coverage and safe rollback or roll-forward. Test rollback after a published
migration and incompatible old binaries. Never restore old credentials, security
grants, owner controls, charges or retained-session authority merely to undo
capacity settings; unknown compatibility blocks rollback explicitly. No ad hoc
backup directories or retention/floor bypasses are permitted.

D9. CLI and private Discord status must show configured mode/overrides, detected
limits, protected reserves, committed demand, currently admissible work and the
limiting resource. Unknown is not zero. Deduplicate unchanged blockers and emit
recovery only from current evidence. Retrying, starting a PID, raising a number,
passing component tests and merging this feature are not successful shipment.
No benchmark is requested. #1703's full CLI/Discord/shipment/soak acceptance stays
in force independently of this capacity feature's tests.
N owns mapping the new resource/deficit fields into the existing versioned
health/progress payloads and outbox. C0 must specify that mapping. N also owns
uncertain-send reconciliation under #1714: keep later updates pending, expose
delivery uncertainty separately from current capacity, and reconcile remote
evidence or use the existing explicit owner-resolution path when ambiguous.
Specify a bounded alert/escalation lifecycle in C0; never silently drop or bypass
an uncertain send to claim recovery delivered. Delivery failure cannot block
ordinary execution admission or turn actual capacity progress into fake failure.

## 3. Verified current code and incident

The November 8 incident had eight retained runtimes charged 1 GiB each against
an 8 GiB policy ceiling. One more required 9 GiB and was refused before model
work. Freeing unrelated build caches did not change that ceiling. A November 9
read-only sample found about 39 GiB free on the state filesystem, 15.5 GiB total
host RAM and nearly full swap. The runtime slice declared an 8 GiB MemoryHigh,
10 GiB MemoryMax, CPUQuota 400% and TasksMax 2048. These are historical evidence,
not defaults or proof of current effective ancestor limits.

Current implementation boundaries:

- `scripts/provision-native-vm-probe.py` initializes new policies with fixed
  max_state_bytes=8 GiB and max_sessions=16 when explicit arguments are absent.
- `scripts/install-autostart.sh` defaults Linux service memory high/max to 8G/10G;
  `scripts/ensure-autopr-worker.sh` also creates the aggregate runtime slice with
  inherited ceilings. Both managed layers need provenance-aware sizing.
- `native_vm.rs` / `native_state_capacity.rs` own native admission, state locking,
  retained count and policy limits. Their existing check does not join the CLI
  storage ledger or its configured free-space floors.
- `run_resources.rs` and the supervisor own actual per-run demand, composed
  native/build guest memory plus controller overhead, and live resource checks.
- `storage_reservations.rs` publishes per-device fixed growth claims; its
  Admission::lock can create metadata/reconcile CI holds, and claims() can remove
  stale records. They are not read-only inspection APIs. Exported claims retain
  their flock lifetime through descendants.
- `build_storage.rs` subtracts other writers' full claims before admitting growth.
  `deploy_snapshot.rs` publishes its peak claim on the snapshot filesystem.
- Updater/provisioning own policy publication/reload; workflow capacity retry,
  health and the private outbox own scheduling and truthful reporting.

## 4. Dependency-ordered GitHub work breakdown and TDD exits

Each task belongs under the capacity epic linked from #1703. C0 is contract work;
C1-C5 are blocked until its relevant contract is reviewed and recorded here.
Every implementation PR records failing assertions against its parent revision,
then green results, tested SHA, affected suites and required hosted CI. Use real
production entry points with disposable filesystems/Git repos, fake clocks and
scripted external boundaries. Subprocess crash tests cover durable changes. Do
not replace behavior tests with source-string assertions or mocks skipping the
entry point. Rust session tests use `scripts/cargo-managed.sh`.

### C0 — Close the host-capacity contract

Owner: resource admission (R), with provider/deploy (P), health (H), delivery (N)
and acceptance (E). Before downstream implementation, define the result schema,
units, resource identity/freshness algorithm, default headroom, effective ancestor
resolution, exact sizing/admission equations, native growth producer, lock order,
policy representation/migration and publication/recovery states. Define override,
shrink/unknown/recovery/hysteresis behavior and each consumer API. Map every
writer, including unresolved CI cleanup and legacy/bound-owner versions.
Include versioned reporting payload evolution and owned uncertain-send
reconciliation/alerting, rather than assuming existing generic progress fields
already carry per-resource deficits.

Exit: no load-bearing placeholder below remains; worked fixtures specify exact
outputs and failure states; the spec passes fresh-reader and complete seam audits.
This task is ready for design work, not a license to invent a second allocator.

### C1 — Detect effective machine resources

Depends on C0. Add failing fixtures for 2/8/16/64 GiB machines; a 64 GiB host with
a 2 GiB ancestor cap; unlimited leaf/limited ancestor; CPU affinity plus fractional
quota; PIDs/FD ceilings; current commitments; differing/shared devices; full logical
VM image with ample backing space; stale/partial input, overflow and mount/process
replacement. Prove discovery changes no policy, claims, CI holds, files or services.

Exit: deterministic bounded observations, explicit per-resource unknown/errors,
and correct effective limits. A cached result cannot authorize a launch.

### C2 — Join native demand to shared storage admission

Depends on C0/C1's storage contract. Start with failing simultaneous native/build/
snapshot admission fixtures on one device and split devices. Exercise duplicate
paths, two state roots, active/exported claims, crashes, unknown cleanup holds,
legacy writers and missing evidence. No reservation is released from age, `du`
or absent processes. Preserve full retained native demand and snapshot floors.

Exit: measuring/reserving/releasing actual demand uses one coordinated authority;
competing admissions cannot spend the same headroom; uncertainty remains held;
ordinary builders/snapshots respect native claims and native allocations respect
theirs. Explicitly verify what physical enforcement exists and its residual limits.

### C3 — Apply automatic deployment defaults and supported migration

Depends on C0-C2. Reproduce the fixed 8 GiB/16-session and 8G/10G initialization
behavior first. Test new installations on small/large/constrained hosts, explicit
overrides, existing fixed policies, current-host auto enablement, unsupported
schemas, filesystem loss, mixed versions, and a ceiling below commitments. Crash
at each file/rename/sync/reload boundary; replay uncertain publication exactly.
Test capacity/service configuration recovery with the existing deploy snapshot
format, including old-binary compatibility and preservation of newer security
revocations/accounting/owner controls.

Exit: supported fresh deployments configure host-derived capacity automatically;
existing deployments switch only through supported migration and preserve all
sessions/charges/holds. Effective worker configuration agrees with the plan after
normal rollout. No raw DB backups, retention/floor overrides or manual restart
shortcut. Installer-only success does not complete this task.

### C4 — Adapt admission while the worker runs

Depends on C0-C3. Fail tests for resource shrink/growth, competing jobs, missing
samples, a lost/replaced mount, stale configuration and oscillating headroom.
Use fake clocks to verify cadence, hysteresis, immediate real pressure refusal,
fairness, bounded wakes and unchanged-input alert deduplication. Assert zero paid
model calls and zero merge allowance consumed by capacity deferral.

Exit: running workers revalidate and adapt safely without operator tuning,
restarting sessions or erasing commitments. Ready work advances when resources
recover through ordinary admission; a larger displayed budget alone is not proof.

### C5 — CLI/Discord visibility and deployed acceptance

Depends on C0-C4; integrate #1914/#1915 rather than duplicate their readers/outbox.
Fail fixtures that confuse retained disk with live RAM, report unknown as zero,
read the observer's environment instead of the worker's, leak private paths/IDs,
overwrite a precise blocker, or report a retry as progress. Test private delivery
reopen/deduplication and uncertain sends.

Exit: inspect the real effective configuration and advance existing authorized
queued work through actual model activity after normal rollout. Validate capacity
shrink/recovery on disposable hosts before production. Publish separate component,
rollout and live-work evidence; keep the parent epic's five-merge, 72-hour fault
and 24-hour unattended requirements open until independently proven.

## 5. Engineering decisions still owned by C0

These are implementation-contract tasks, not unanswered requests for the owner
to pick arbitrary machine limits. C0 blocks dependent coding until they are
specified and audited:

1. Exact default headroom and resource-vector sizing equations; how current
   commitments and complete per-run demand interact without greedy pool capture.
2. Native participation in the existing per-filesystem ledger, durable lifetime,
   original release proof and cross-version compatibility. No inferred discount.
3. Observation identity, freshness/clock rules, ancestor resolution, coherent
   reads and allocation-lock revalidation, including replace/remount cases.
   Distinguish all verified installer-managed output scopes from external limits;
   specify provenance, bootstrap order and post-reload readback.
4. Stable auto/fixed configuration representation; supported legacy and bound-
   owner migration; complete pending/uncertain/applied/effectively-loaded states.
   Ambiguous legacy settings require an explicit scoped auto-mode operation,
   never guessing provenance from a numeric value.
5. Precise pressure/unknown/recovery reasons, hysteresis, alert lifecycle and
   current-worker effective readback, with public/private output boundaries.
6. Actual disk-growth enforcement coverage and residual exhaustion/recovery for
   writers outside coordination. Accounting reservation is not a physical quota.

## 6. Non-goals and glossary

No new workflow engine/independent allocator, self-hosted public-PR runner,
archive migration, privileged freeze/quota rollout, reservation discounts,
cleanup by age, weakening of independent review/CI/owner controls, or benchmark.
No claim that a sample excludes all future uncoordinated writes.

Retained state: disk state kept across turns, separate from live execution RAM.
Effective limit: tightest relevant host/ancestor/operator limit with protected
headroom not yet subtracted. Usable envelope: effective limits after the one
specified headroom/commitment subtraction. Commitment: existing authority's declared reserved demand,
not current file size or RSS. Auto: deployed sizing/admission behavior; inspect:
a read-only command showing that behavior. Observation: data, not a launch grant.

## 7. Tracking and audit status

Dedicated capacity epic: [#1917](https://github.com/nolanmak/Jarvis/issues/1917), a sub-issue of #1703.

- C0: [#1918](https://github.com/nolanmak/Jarvis/issues/1918) — Contract/design closure.
- C1: [#1919](https://github.com/nolanmak/Jarvis/issues/1919) — Effective resource discovery.
- C2: [#1920](https://github.com/nolanmak/Jarvis/issues/1920) — Shared native/build/snapshot admission.
- C3: [#1921](https://github.com/nolanmak/Jarvis/issues/1921) — Deployment defaults and migration.
- C4: [#1922](https://github.com/nolanmak/Jarvis/issues/1922) — Runtime adaptation.
- C5: [#1923](https://github.com/nolanmak/Jarvis/issues/1923) — CLI/Discord visibility and live acceptance.

C0 is design-ready. C1-C5 have native GitHub blocked-by dependencies and remain
blocked until their predecessors and reviewed contracts satisfy their exits.
The previous read-only-default proposal is superseded. A fresh-reader review and
all 56 mapped seams were checked for this issue-plan handoff. Findings were
resolved in the plan or explicitly assigned to C0; this does not make the
dependent implementation ready or establish deployment. See the
[planning audit record](host-capacity-sizing-audit.md).

Documentation review: [PR #1916](https://github.com/nolanmak/Jarvis/pull/1916).
The document is on that review branch until normal merge; it is not a deployed
feature or an exception to independent code review and required CI.


### Audited shared-authority prerequisite

[#1930](https://github.com/nolanmak/Jarvis/issues/1930), under C2 #1920, may
extract the existing admission/CI cleanup authority into channel-core under the
[closed extraction contract](shared-storage-extraction-contract.md). Fresh-reader
and all 28 mapped sub-seams were checked, including source counterparts. The
implementation preserves current semantics and moves checkout ownership tests
with the authority; it adds no native charges or activation. This scoped
exception does not unblock full C0/C2, durable native enrollment or deployment.

Implementation [PR #1931](https://github.com/nolanmak/Jarvis/pull/1931) at
b4a77220 passes 41 selected local core/CLI cases, including all 18 moved tests.
All 25 current-head hosted checks/statuses pass after the pinned downloads
recovered; independent code approval, merge and rollout are pending. This component does
not deliver native enrollment or activate machine-sized capacity.


### Native durable balance checkpoint

The [balance/replay amendment](native-storage-ledger-contract.md) specifies
proposed tables, complete graph/replay checks and lifecycle constraints in the
existing native database. Fresh-reader and all 16 mapped seams were reviewed;
all 40 disposable SQLite predicates pass. This closes the identified
representation ambiguities at checkpoint scope, not full C0 or a production
implementation exception. Original proof, registry/migration, full producer
and pre-intent control-plane growth remain load-bearing gates.
