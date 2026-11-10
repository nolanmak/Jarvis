# Review retry lifecycle: source discovery for #1952 C2

Status: source inventory and required tests, **not an implementation-ready
contract**. Owner #1709 / #1952, scheduling #1706, private reporting #1714,
acceptance #1715, parent #1703. This discovery
uses implementation branch head `7d865307b03a0206fb1f1d0764e0e08009a685f0`
(main base `416f392b17b56b2a43e67a3e430835902387941a`). C1 #1954 has published-input hosted GREEN at 3debdb8d and remains
unmerged; its original attempt is terminal after 13 model admissions, and its
workflow is normally paused without an active lease. See the
[terminal discovery](1703-pr1954-terminal-acceptance-discovery.json). The original
request, charges, published input, candidate history and pinned verifier remain
preserved. A pause does not prove teardown. This document grants no
implementation exception.

The [coherent source contract](review-dependency-recovery-contract.md) separates
successful bounded recovery (C1) from failed observations, unchanged paid
retries and wake semantics (C2). Those obligations remain even if C1 supplies
all requested items. No benchmark, approval bypass or allowance reset applies.

## Verified producer and consumer boundaries

1. `self_improve_review.rs::run_for_subject` catches `run_inner` errors and
   returns `unavailable_from_error`. `IndependentReview::unavailable` in
   `self_improve.rs` creates `structured: None`. Thus an error after a first
   valid incomplete response can lose its structured findings from the returned
   result. Completed fragment caching is a different mechanism: it happens
   only after `execute_evidence_group` succeeds. It does not preserve the failed
   group's pending findings or establish a completed observation from a lost
   response. C1's in-attempt GroupReview retention does not close this seam.
2. Issue-backed resume in `self_improve.rs` records any supplied structured
   review and updates `previous_review` **before** matching unavailable. The
   missing producer cannot be repaired by adding another identical recorder.
   Its QA reasoner call precedes independent review. A hold checked only inside
   independent review would still permit an unchanged paid QA call on every
   resume. Placement of the no-spend check must cover the whole paid path.
3. Source-less `self_improve_adopted.rs` sets `previous` when structured evidence
   exists, records a publication verdict only when `recorded_verdict` returns
   Some, then calls `episode::evaluate`. The evaluator already writes any
   structured evidence as `episode-evidence-*` even when unavailable.
   `recorded_verdict` deliberately returns None while unavailable. Preserve
   that separation; partial facts must not become a publication verdict.
4. `autoship_review_epoch.rs::invalidate` advances a workflow event floor and
   clears current review, fragment and ready indexes. It preserves historical
   workflow events. `current_review_history` returns no current
   review history while owner messages are pending, reads only after the floor, stops at generation/owner boundaries and requires
   the exact current checkpoint binding. `episode_review_at_head` uses that
   current-authority reader. Neither is a historical-fact import API. Do not
   weaken them or `verify_fresh_review_origin` to make old evidence visible.
5. `ReviewUnavailable::EvidenceBudget` has a precise code but `permanent()` is
   false. `unreviewable_plan` asks for evidence decomposition and preserves the
   draft; `note_unreviewable` counts days and stores code/headline. These records
   contain no complete source/contract/protocol fingerprint or authoritative
   no-model admission transition. A day counter or a public comment is not an
   unchanged-input hold. Existing verification/capacity retry mechanisms cover
   their own domains; do not silently treat them as review-input authority.
6. `WorkflowStore::record_review_fragment` checks the current lease/binding,
   audit generation, immutable payload and 8 MiB bound in a transaction. Its
   completed-fragment semantics must not be reused for failed/uncertain calls.
   Durable review events and native operation admission are existing ownership
   boundaries; any new projection must join them rather than create a parallel
   session, approval or release ledger.

## Required behavior-first tests for the next contract

- Drive the real adapter through a valid initial finding, then a deterministic
  missing/oversized dependency. After failure and restart, recover the exact
  finding, original binding, actual observation identity and source provenance
  as non-approval facts. The result remains unavailable; no fragment completion,
  merge-ready receipt or recorded publication verdict can be fabricated.
- Run the issue-backed and source-less entry paths through that failed result.
  Demonstrate their explicit historical-facts reader receives the original
  separate non-approval observation; current structured/publication hooks must
  not receive it as ReviewEvidence. Do not merely test a serializer or duplicate
  an existing successful-review recorder. Advance the owner epoch/head and prove
  historical facts remain inspectable outside current approval history.
- Repeat the same durable unsupported input through the actual command worker
  and resume path. Assert zero **additional actual model admissions**, including
  QA/scoping/acceptance repair before the reviewer. Preserve command identity,
  candidate and findings. Do not substitute tokens=0, an unbilled label, a day
  counter, sleep or completion of the command for this observation.
- Change source/contract/owner epoch/reviewer model/protocol/policy independently.
  Verify the settled wake predicate, current authority revalidation and exactly
  one permitted wake under concurrent consumers/restart. An unchanged timer,
  title, generic status update or duplicate request must not erase the hold.
- Inject transport/admission/storage failures and a lost native response or
  failed result publication. Distinguish those from deterministic source
  refusal; reconcile original operation uncertainty before authorizing another
  paid call. Neither absence of a result nor an expired lease proves no spend,
  native retirement, resource release or permission to reissue.
- Verify public CLI/private Discord report the same durable blocker/wake state
  with sanitized detail; notification delivery cannot clear the hold. Diagnostic
  runs still reserve their immutable call budget and require fresh actual
  observations instead of borrowing a production failure or approval cache.

## Load-bearing open threads before handoff

- R: exact deterministic-versus-transient failure taxonomy and producers. C1
  still uses contextual errors for several unavailable-source cases; strings
  must not become the authority for a persistent no-spend decision.
- R/E: failed-observation schema, bounded source/findings, original binding and
  native observation proof, crash/publication ordering, and an explicit
  non-approval historical reader/resolution transition. Existing event history
  alone does not supply those validation or retention guarantees.
- R/P: exact input fingerprint and wake matrix, including the reviewer selection
  and protocol revision. Specify whether a changed input may retry the original
  operation, start a new observation, or must first reconcile uncertainty.
- R: durable lease-fenced hold/claim API and placement before every affected paid
  entry path, with actual worker projections, fairness and restart behavior.
  Admission must not bypass existing owner, capacity or native authority.
- H/N/E: report schema compatibility, sanitization, delivery lifecycle and real
  command/Discord acceptance. Define fixtures with exact events and call counts.

Close and audit these threads against the original native operation and workflow
producers before implementing C2. Full #1952 closure also requires capture/replay
of PR1944's actual typed request after normal C1 rollout; a made-up source pair
cannot stand in for that incident. Automatic host capacity and all #1703 live
acceptance remain separate unfinished requirements.


## Working C2 decisions after source recheck

[Working lifecycle contract](review-retry-lifecycle-contract.md) chooses a
separate failed-observation type and pre-dispatch hold identity, with an explicit
wake/revalidation matrix. It remains unready for implementation. Both callers
consult adjudicated amendment before unavailable handling; its complete-role
validator must remain intact. The existing GitHub operation table is not a
native observation ledger. Exact native dispatch/result custody and complete
paid-path admission mapping are the next load-bearing source tasks.

### Source follow-up: authorization before workflow and paid admission

On main416f392b, self_improve_targeted::read turns every unsuccessful gh lookup
into a generic target-lookup error. work_once_mode wraps authorize_command
with CommandFailure(code2,target-authorization-failed). CLI run/control also
map authorization errors to that denial. Discord control and owner-message
ingress map errors to the same stable refusal; owner inbox permits that
refusal value. Earlier recorded production quota failures are owned by #1706.

This is a separate source boundary from a known deterministic evidence hold.
A failed remote lookup cannot establish either a revoked grant or positive
authority. R/S/#1706/#1712 must map pre-workflow submit, existing-command worker,
CLI control, Discord control and owner-message ingress, including pending
authorization, replay and retry behavior. Do not silently finalize a transient
lookup failure as permanent denial, treat it as owner authorization, allocate
a new request ID to evade its recorded receipt, or spend models to reconstruct
the missing trusted inputs. Exact typed results and receipt transitions remain
implementation gates; this inventory does not introduce another ingress engine
or override the existing owner-turn/final-dispatch guards.


## Native identity retention source clarification

A follow-up source read at main `4ebb2b74` confirms that `Lease::observe` writes the actual observed ID into `NativeSession::State.id` immediately. `Lease::Drop` can mark that state uncertain without clearing its ID. The loss boundary is therefore the current `fresh_review_scope` return/lifetime interface and durable publication ordering: its early `await?` exposes neither the failed outcome/session handle to its caller, and later `NativeSession::Drop` retires registered paths. Do not invent a replacement native ID or change observation semantics on the assumption that failure erases the already observed ID. A complete custody interface still needs original accounting/resource correlation, bounded durable response/source storage, uncertainty reconciliation and retirement fencing before activation. This source clarification supplies none of those missing authorities.
