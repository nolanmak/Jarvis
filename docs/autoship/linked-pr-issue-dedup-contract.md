# Linked PR claims and fresh issue selection

Status: investigation draft for #1942, under #1537/#1707/#1703. This is not
implementation handoff approval, a code review, or deployed behavior.

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
Malformed or missing metadata necessary to determine trust/source must not
silently become an empty inventory. Exact classification and fixtures for mixed
trusted/untrusted/malformed rows remain an open thread below.

D5. Cover primary background selection, explicit CLI/Discord issue targeting,
red-main planning and configured remote repositories. Targeted execution already
calls `has_open_agent_pr` and reports `target-pr-reconciliation-required` for
unbound claims. Preserve that safe routing instead of choosing an arbitrary PR.
The red-main caller currently treats lookup errors as resume, so typed unknown
results must remain unable to dispatch a fresh implementation through that path.

D6. Recheck after selection before paid scoping and immediately before a new
publication. Selection is only a point-in-time observation. A PR appearing in
between must stop duplicate work at the next boundary and retain the current
candidate/checkpoint for reconciliation. Existing same-identity pending
publication recovery must continue to reconcile its exact original PR; a general
issue claim cannot redirect or discard a dispatch that may already have happened.

D7. This repair must report stable safe blocker codes through existing private
status delivery. It must not close, relabel out, reset or delete another issue,
PR, workflow, candidate, saved verifier or retained runtime. It gives no allowance
credit and does not change automatic host sizing.

## Source boundaries requiring explicit closure

- Primary selection calls the guard before author trust filtering. Fresh
  `run_selected` subsequently constructs a reasoner, prepares a retained checkout
  and acquires the workflow before scoping. The new dispatch recheck must occur
  before any paid model call and preserve that workflow's existing binding.
- `publication::publish` saves PendingPublication and a CreatePr intent, validates
  the binding, then reconciles direct known identity or branch/head history. Only
  the no-known-publication branch can consider a new create. Integrate a fresh
  issue-claim recheck there without invalidating lost-response recovery.
- Remote selection also has a store-level `has_open_agent_pr_run` check. Preserve
  it and determine the remote scoping/publication boundaries before handoff.

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

## Open threads, owner and trigger

All are owned by the #1942 implementer and block implementation handoff:

- Define the strict claim observation/result schema and safe reason/action codes,
  including missing metadata versus known untrusted rows and canonical identity.
- Close remote trust, paid dispatch and publication routing with actual callers.
- Define same-workflow identity exclusions at publication, including preexisting
  intents with unknown remote effects and claims linked to multiple issues.
- Document the unavoidable remote race: there is no atomic GitHub operation
  locking an issue to one PR. Boundary rechecks narrow that window; do not claim
  complete prevention for a concurrent external create after the final read.
  Specify reconciliation and tests without destructive automatic cleanup.
- Complete fresh-reader, conversation/source and mapped seam audit; record real
  RED before behavior changes. No such audit or test run is claimed here.
