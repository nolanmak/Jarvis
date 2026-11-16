# CI diagnosis and bounded source repair (#1710)

This delivery advances the first two original acceptance bullets: replay the
#1669 failure shapes into diagnosed repair or an explicit dependency, and keep
introduced failures behind bounded source repair, new-head verification and
independent review. It does not close #1710 or claim production acceptance.

## Historical evidence

PR #1669 is now merged and green. The original failing source was
`fd072a78b30fed89b05f8f29c48545becc0b85cc`, dated October 29. Read-only inspection
captured these completed jobs and bounded failed-step logs once:

| Job | Evidence | Diagnosis |
| --- | --- | --- |
| decisions-and-context | [123782480594](https://github.com/nolanmak/Jarvis/actions/runs/40969526643/job/123782480594) | Cargo refused to update Cargo.lock with `--locked` |
| bridge-suites | [123782480629](https://github.com/nolanmak/Jarvis/actions/runs/40969526646/job/123782480629) | Native supervisor exited before its controller-admission handshake |
| node-release | [123782480950](https://github.com/nolanmak/Jarvis/actions/runs/40969526604/job/123782480950) | npm audit reported moderate dependency advisory GHSA-rmxm-3fg6-px4f |

These causes are not classified as flaky, introduced or shared merely from a
failed status. Attribution requires paired evidence. The historical
approval-history workflow was retired; its lockfile diagnostic is replayed
under the current pinned targeted-job identity. The fixture does not fabricate
historical workflow approval under today's contract.

## Execution path

The existing resume workflow observes hosted CI before paid acceptance
preparation. The embedded hosted reader remains the policy authority. C binds
each failed Actions job to its verified workflow/run/attempt/head, captures at
most five failed jobs, and reuses the existing streaming log reader's bounded
head/tail capture. Missing identity, missing logs, unknown evidence and diagnostic
bounds produce explicit dependencies. Advisory review workflows remain advisory.

For a supported job, C exports literal candidate and current-main source through
the existing snapshot machinery and uses the existing build VM and pinned #1616
runner. Both plans must have identical commands, declared environments,
prerequisites and deadlines. Each job comparison has a 900-second execution
budget, within existing workflow/resource limits. Candidate source, main and
bindings are checked around replay. A local comparison never creates a passing
CI receipt.

A reproduced candidate failure with green main enters the existing guarded
revision path. A matching main failure produces `ci_main_failure_dependency`;
an unavailable or inconclusive comparison remains a dependency. Private repair
notes contain bounded diagnostic excerpts; controller records contain typed S
outcomes, concrete job identities, actions and evidence hashes. They omit log
bodies. R capacity refusals retain their original error and capacity checkpoint.

Two repair reservations are allowed per protected contract and stable failure
family through the existing durable epoch budget. A changed head, changed log
paths/timestamps or controller restart does not replenish that budget. Actual
provider usage and shipping allowance retain their existing accounting. An
unchanged repair cannot create an empty publication commit. Integration or
retained edits supersede the old-head diagnosis and follow normal new-source
verification. Existing acceptance, exact-head independent review, hosted checks
and merge protection remain mandatory after repair.

## Verification

The parent regression calls the actual resume findings helper and hosted reader
against a real disposable Git repository and scripted GitHub. It fails because a
confirmed lockfile failure becomes an unclassified builder prompt. The original
RED fixture is retained in commit `ce8a400c`; the adjacent JSON records its exact
parent, command, exits, assertion and log hash. A fixture-setup failure before
that run is excluded from RED evidence.

Additional fixtures compile a real API crate and unchanged dependent caller.
Changing the public return type produces E0308 with green baseline. The C
production preparation entry uses that real compiler result at a scripted CI
execution boundary, then exercises protected repair admission. Shared failure,
missing logs, unsupported diff-dependent recipes, retained edits and moved
remote heads remain explicit safe outcomes. A repaired caller compiles but
cannot reuse old independent review or waive hosted CI. A subprocess is killed
with SIGKILL after durable reservation; fake lease time, a changed head and
reacquisition prove the same finite family budget remains spent. Capacity-cause
loss was caught as an implementation regression and repaired.

The one full CLI attempt on source `a5ac9a30` completed with 2,177 passed,
47 ignored and one existing structural-test failure: its commit counter matched
only `--allow-empty` syntax. The test-only correction also counts the deliberate
non-empty CI repair commit while retaining five commits/five publications.
The affected CI/workflow/publication recheck then passed 43 tests, zero failures,
with four existing live-environment ignores. Production logic was unchanged after
the full attempt; no second full suite was run. Required CI on the PR head remains
an independent shipping gate.

All Rust checks use `scripts/cargo-managed.sh`, the assigned L2 target, jobs=2,
disabled dev/test debuginfo and test-threads=2. The launcher restores inherited
nohup SIGHUP to default in its child only and retries managed admission for up to
45 minutes. The compiler fixture executes trusted fixture code inside that
managed test run; it is not production VM acceptance or a host execution fallback.

## Follow-up scope

Remaining work stays on #1710 and its linked owners:

- Canonical shared-baseline repair-issue deduplication and waking every dependent PR.
- A complete missing/skipped/cancelled/stale/transport state projection, including
  all matrix and additional branch-protection app identities; existing merge
  refusals remain intact.
- The one automatic transient rerun policy and complete durable rerun/dependency
  wake semantics. This slice sends no automatic GitHub reruns.
- Frozen diff-dependent selection and matrix replay. Such jobs currently report
  explicit selection/recipe dependencies instead of comparing different suites.
- Direct H/N consumption of the C producer records.
- The appended #1805 scope-correction and original-verifier environment recovery
  incidents, including the finite family/amendment contracts with S and #1799.

Production configuration, databases, holds and services are untouched. The
orchestrator owns branch updates and protected merge; Nolan/orchestrator owns
normal snapshot/updater rollout and current-build live acceptance afterward.
