# Linked PR claims and fresh issue selection

Status: design-audited bounded repair contract for #1942, under #1537/#1707/#1703.
Implementation may proceed after the original behavioral RED is recorded. Design
review is separate from independent code approval, merge and deployed behavior.

## Observed failure and existing authority

On main 416f392b, PR #1937 was open and ready at 09:36:36 UTC on November 9,
2026, closing #1936. Fresh scoping subsequently created #1938/#1939 at
09:48:29/09:48:46 UTC. The parent recorded 12 model calls without a PR milestone.
The original production command for PR #1937 remains its own workflow; do not
reset it to demonstrate this repair.

`self_improve.rs::has_open_agent_pr` checks pending publication and only the
`agent-fix/issue-N` head. `has_open_agent_pr_remote` repeats that branch-only
lookup for configured repositories. Both turn malformed successful JSON into
an empty array. In contrast, `find_resumable_draft` already uses complete open
PR inventory and `self_improve_adoption.rs::authorized` already recognizes
trusted same-repository closing references on arbitrary branches. Adoption is
present; the fresh-work dedup path does not share its source facts.

## Decisions and required behavior

D1. A claim suppresses new implementation; it does not grant execution, review,
merge or permission to retire an issue. Reuse the existing adoption trust and
repository/source mapping. Preserve exact-head independent review and all owner,
dependency, capacity and policy holds. Never derive approval from a branch name,
author login alone or model-written prose.

D2. Obtain complete open PR inventory through the existing repository connection
helper. Do not introduce search/label/head filters that hide other authorized
branches. Preserve bounded expansion and refusal at the existing inventory
ceiling. Failed, malformed, incomplete or internally conflicting inventory is
unknown, never an empty absence allowing paid fresh work.

D3. Check explicit canonical same-repository closing references and the existing
legacy branch fallback. Share reference validation with adoption rather than
inventing a second parser. A single PR closing multiple issues must suppress
fresh work for each valid claim without choosing one issue as its execution
source. Several PRs claiming one issue also suppress fresh work; they do not
implicitly select which PR to execute. Existing holds remain observable.

D4. Known forks and known untrusted authors cannot reserve issues by assertion.
Malformed or missing metadata necessary to determine trust/source produces
`linked_pr_inventory_unknown`; no model/create side effect follows. For every
row require a valid unique PR number (the existing complete-inventory helper
already does this) and an explicit boolean same-repository flag. Skip known
forks before inspecting their author or references. For same-repository rows,
require nonempty author login; use the existing trusted-author function with the
configured allowlist plus the explicit target repository owner, as adoption does.
Use an absent association as the existing empty association, not fabricated
write access. Known untrusted authors can be skipped. For trusted rows require
nonempty branch/head/base strings and a closing-reference array whose elements
have positive numbers and nonempty URL strings. Canonical same-repository URLs
supply claims; well-formed foreign URLs do not. Preserve duplicate references in
the adoption mapper; deduplication only collapses the suppression set.

The shared extractor must preserve adoption's current permissive filtering for
its existing callers; the new suppression observer validates the required fields
first and then calls that extractor. Do not change AdoptedPr's serialized shape
or grant execution to a previously ambiguous multi-source PR.

D5. Cover primary background selection, explicit CLI/Discord issue targeting,
red-main planning and configured remote repositories. Targeted execution already
calls `has_open_agent_pr` and reports `target-pr-reconciliation-required` for
unbound claims. Preserve that safe routing instead of choosing an arbitrary PR.
The red-main caller currently treats lookup errors as resume; the actual Resume
branch returns held when no guarded PR is available, without falling through to
fresh work. Preserve that safety and report unknown as unknown rather than
claiming an open PR was confirmed.

D6. Recheck after selection before paid scoping, after scope before acting on
its output, before subsequent fresh builder/acceptance dispatch or decomposition,
and immediately before a new publication. Selection is only a point-in-time observation. A PR appearing in
between must stop duplicate work at the next boundary and retain the current
candidate/checkpoint for reconciliation. Existing same-identity pending
publication recovery must continue to reconcile its exact original PR; a general
issue claim cannot redirect or discard a dispatch that may already have happened.

D7. This repair must report stable safe blocker codes through existing private
status delivery. It must not close, relabel out, reset or delete another issue,
PR, workflow, candidate, saved verifier or retained runtime. It gives no allowance
credit and does not change automatic host sizing.

## Observation, holds and source boundaries

D8. One new observation function receives explicit target repository and issue
identity, obtains the existing complete PR connection, validates it as in D4,
and returns either confirmed absence, a nonempty set of claiming PR numbers,
or typed unknown. A claim has reason `linked_pr_claim_exists`, next action
`reconcile_existing_pr`; unknown has reason `linked_pr_inventory_unknown`, next
action `retry_github_inventory`. No raw GitHub output/private title/body enters
these codes. The old bool adapters may return true for claims and an error for
unknown, never false for uncertainty. Preserve pending-publication checks and
store-level remote run ownership independently of this observation.

D9. The existing complete PR helper expands 500 to at most 10,000 rows and
refuses a full final page. Nested closing references have another bound: the
installed GitHub CLI query selects only the first 100. For trusted rows with
100 or more references, return unknown rather than treating that list as complete.
This deliberately refuses fresh admission from a saturated nested connection;
there is no unbounded per-PR hydration in this repair. Direct explicit PR execution
continues through its existing guarded path. A future bounded nested-reference
pager can lift this limitation with its own completeness/replay tests.

Verified against installed CLI 2.86.0-112-gc30647b78 and its pinned upstream
[query builder](https://github.com/cli/cli/blob/c30647b78/api/query_builder.go#L177).
`pr view` has separate hydration in its finder; that does not prove that `pr list`
hydrates every row. The observer cannot infer complete references from the full
outer PR inventory. This bound is independent of the number of authorized PRs.

D10. Primary background selection uses the bool adapter and skips claims; unknown
stops fresh selection. Explicit issue targeting preserves its existing
`target-pr-reconciliation-required` refusal; error details are stable safe codes.
Red-main planning must hold on unknown with its precise reason instead of
reporting a confirmed open PR through the current conservative Resume fallback. Existing linked-PR adoption/resume remains its
own authority and preserves the source/dependency ambiguity rules.

D11. Fresh `run_selected` must recheck before constructing/dispatching fresh work,
after the initial scoper returns and before acting on that scope, before a new
acceptance/builder dispatch, and at actual decomposition child-creation boundaries. Existing
`self_improve_readiness.rs::decompose_unverifiable` may generate a plan through
`scope_authority::persistent` before calling `file_slices`; that paid planning
dispatch is included. Actual child creates and the final parent-label edit pass
through `self_improve_slicing.rs::Transport::call`; gate those mutations with the
parent issue, including recovery that reuses an admitted plan.

Claim/unknown errors must propagate as typed holds through `file_slices` and
terminate the oversized-scope caller before its existing catch-all slicing-error
warning falls through to generic scope_deferred, backoff comment or label-out.
The same rule applies to decomposition and publication callers that catch errors.
Test a valid oversized plan with a PR appearing immediately before child create:
no child dispatch, no parent comment/label mutation, unchanged retained plan and
candidate, and the exact blocker/action surviving command completion.
A claim discovered midflight keeps candidate/checkpoint and stops subsequent
fresh paid/mutating work. Do not reset criteria, baseline, builder history or
workflow evidence. A negative observation is short-lived data, never a durable
license to spend or publish later without the required recheck. Tests must script
PR appearance after scope, not only after initial selection.

Where a WorkflowGuard exists, persist the reason/action on its current nonterminal
stage with unchanged binding/evidence. Repeated identical holds must not advance
stage or refresh progress. On first recording the hold, set eligibility to observation time plus five
minutes. Identical observations preserve that timestamp; later reads may remain
eligible for free observation but do not spend models. New known
absence allows normal guarded recovery; owner pause and existing authorization
checks retain priority. Extend the existing progress allowlist/projection for
these two precise reasons, including generic command-finish overwrite tests.
Before guard acquisition, return a held report/error with the same safe code;
do not create unrelated workflows just to manufacture a status checkpoint.

D12. `publication::publish` already saves PendingPublication and CreatePr intent,
then reconciles known direct identity or exact branch/head history. Preserve those
paths before any issue-claim test: they never create again from a known identity.
Only when exact reconciliation has found no publication, immediately before
reserving another create dispatch, observe current issue claims and require
confirmed absence. A claim/unknown retains the pending intent, dispatch history
and candidate. This applies even to a retry whose previous create response was
lost: stop a new dispatch without pretending the old dispatch never happened.
Never redirect its identity to a different linked PR or delete pending evidence.
Same-branch/head PR found by the original reconciler continues to settle normally.

D13. The configured-repository path has two additional actual boundaries.
`process_one_repo` selects through `pick_issue_for_repo`, creates a branch and
calls `reasoner.call_with_storage`; recheck after selection and before that call.
`open_approved_runs` runs later, revalidates allowlist/enabled state, then creates
a draft PR from a persisted approved run. Recheck the target repository/issue
there immediately before `gh pr create`. On claim/unknown preserve the approved
run, branch and existing approval, report reconciliation/unknown, and do not mark
it failed/opened or erase evidence. This does not add remote auto-merge or pretend
its older publication path has the main workflow's dispatch ledger.

D14. A remote GitHub observation and external PR creation are not atomic. A
concurrent external PR can appear after the last absence observation. This repair
structurally blocks new dispatch at every defined observed-claim boundary, but
cannot promise universal distributed uniqueness. The next normal inventory
reports the conflict and holds further fresh work; it does not auto-close either
PR or resolve ownership from text. Tests must distinguish an observed-before-call
claim (no dispatch allowed) from an after-call race (preserve and reconcile actual
results). A GitHub/server-supported atomic issue-claim facility, if introduced
later, requires a separate contract and does not excuse the current checks.

## Behavior-first delivery and evidence

1. RED: exercise the existing real issue-selection entry with subprocess fake
   GitHub and disposable workflow state. Return no legacy-head PR but an open
   trusted arbitrary-branch PR explicitly closing the issue. Assert zero scoper,
   builder, decomposition and create calls. Record old-code failure, not an
   absent newly invented helper or a source-text assertion.
2. Table cases: primary/remote and explicit issue targeting; unrelated/foreign
   references; legacy mapping; one/many source claims; fork/untrusted attempts;
   missing metadata; malformed/duplicate/truncated/failed inventory; closed PRs;
   dependency/owner holds and changing heads. Assert guarded resume remains
   available without approval being fabricated.
3. Race cases: scripted inventory changes between selection, paid dispatch and
   create. Assert no subsequent paid/create call, preserved candidate evidence,
   and original pending/known publication reconciliation after lost responses.
4. GREEN: managed local tests where admitted, relevant hosted tests on the exact
   pushed head, independent code approval and normal updater rollout. Record
   new original workflow observations separately from component tests. Do not
   restart #1937 or claim an external merge as autonomous shipping.

## Glossary and non-goals

Claim means observed authorized issue association that prevents duplicate fresh
work. Execution source means the existing unambiguous workflow/adoption contract.
An inventory is an observation, not a lock over GitHub. This change adds no
second ownership database, distributed lock, allocator, review bypass, automatic
closing policy or capacity override.

## Build order, deferrals and open threads

The #1942 implementer owns delivery. First record the original guards' behavioral
RED (draft #1945 at 97444937 has three pending subprocess regressions); then share
reference extraction and implement the strict observer/adapters; then wire all
actual paid/decomposition/publication boundaries and precise holds; finally run
the broader entry/race/progress suites, hosted CI, independent code review and
normal rollout. Guard-only GREEN is not the complete issue's exit.

D9's nested-reference hydration is explicitly deferred. Carrying cost: saturated
trusted PR connections hold fresh admission until that PR becomes unsaturated or
a bounded pager is implemented; no schema/backfill is required because this is
an observation, not persisted ownership. Trigger: a real saturated connection or
a follow-up implementing complete pageInfo/cursor tests. D14's residual external
race is accepted with boundary checks, preserved original effects and normal
conflict reconciliation; it ends only with an actual server-side atomic facility.

## Design audit and remaining delivery gates

Fresh-reader audit, conversation/source comparison and all 26 mapped narrow
seams were checked. The original red-main premise was corrected against its
actual always-returning Resume consumer. One fresh-reader eligibility wording
finding was resolved by specifying the first observation timestamp. One narrow
seam finding identified the oversized-scope slicing-error consumer that could
label out a held parent; D11 now explicitly requires typed propagation and its
actual-caller regression. S14/S15 were rechecked after that amendment. The
historical runtime receipt's provenance was added and S26 rechecked. All 26 seams
hold at this scoped design level; this is not independent code approval.

Delivery remains unproven: confirm real behavioral RED, implement all observation/
paid/decomposition/publication/status boundaries, execute the matrix and original
process/recovery fixtures, pass exact-head hosted tests, obtain independent code
approval, merge and observe normal rollout. The actual primary and remote selector
entry points have been identified in source and test definitions; their execution
is still pending. Full #1703 capacity/productive-owner/live acceptance is separate
and remains open. No Rust GREEN, behavior fix, rollout or restoration is claimed.

## Additional live observation

The [observation receipt](1703-linked-pr-dedup-observation.json) records the
original read-only runtime sample and corroborating public creation times.
At 2026-11-09T11:13:22Z the original worker held active issue workflow
`nolanmak/jarvis#1943` at Build/criteria_registered, seven recorded model calls,
current heartbeat and about 50 minutes since stage progress. The ready repair
PR #1944 was separately queued with no worker workflow yet. The issue attempt
began before PR #1944 was opened; this is evidence for a midflight appearance
case, not proof that initial issue selection missed an already-open PR. Nested
build QEMU was live with about 730 CPU seconds; heartbeat alone is not productive
model progress or a reason to kill/reset the original process.
