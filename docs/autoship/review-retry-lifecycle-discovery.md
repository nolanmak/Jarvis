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


## Main 4ebb2b74 paid-path source map

This source pass identifies the production entry families and their early paid
work. It does not define a hold schema, settle native response custody, or make
the C2 contract implementation-ready. The paths below were read at full main
`4ebb2b745c62476ad08571dea008307f2235a986`; line numbers are only discovery aids.

| Entry family | Existing ownership before first paid work | Early paid work that a deterministic hold must precede |
| --- | --- | --- |
| Worker fresh issue / explicit issue command | `self_improve.rs::run_selected` prepares the candidate, computes source/environment/issue contract, acquires `WorkflowGuard`, binds builder, and records `trusted_request_acquired` | `readiness::assess`, original scoping, optional slicing, initial acceptance construction, then builder. Checking only before the final independent review misses these calls. |
| Issue-backed draft resume / explicit linked PR | `resume_draft_pr` authenticates/import-preflights, locks and prepares the preserved candidate, acquires its workflow and binds builder | `prepare_acceptance` runs before reconciliation, verification repair and the paid QA calls near lines 7047 onward. A same-input hold must be checked before preparation, not after QA. |
| Source-less adopted PR / explicit unlinked PR | `self_improve_adopted.rs::resume` acquires workflow at the preserved PR and binds builder before `execute` | `readiness::assess_pr` precedes acceptance construction; conflict repair uses `adoption::revise`; acceptance repair and independent review follow. A hold inside `aligned_review::run_for_subject` is too late. |
| Acceptance repair and amendment | Existing caller supplies its acquired guard and prepared registration | `self_improve_contract::verify_with_repair_for` can call independent `amendment::try_amend`, builder `adoption::revise`, then verification again. A new loop round or amendment proposal cannot silently create a new paid episode. |
| Review / alignment / final adjudication | Existing caller supplies the workflow audit and immutable subject binding | `self_improve_review::execute_evidence_group`, `self_improve_alignment`, `self_improve_adjudication_windows` and constraint learning each enter fresh native reviewer scopes. A successful call boundary still does not supply failed-result custody. |
| Episode re-scope / review repair | Existing guarded candidate and epoch are passed by the owning pipeline | `self_improve_episode` can call a fresh builder for re-scope; adoption's `revise` wraps the builder's storage-aware call. These are paid calls inside the episode, not independent free wake attempts. |
| Productive owner follow-up | The store has owner-turn claiming, revision fences and conversation readers | Main has no production consumer of `claim_owner_turn`/`WorkflowGuard::acquire_owner_turn`; observed consumers outside definitions are tests. There is no current productive Discord coding path to certify. #1713 must wire this path through the same early hold and final dispatch fences when implemented. |

The shared launch fence is installed by
`self_improve_workflow::WorkflowGuard::install_model_accounting` at guard
acquisition. Its dispatch observer obtains the current stored binding and calls
`WorkflowStore::validate_dispatch`, which validates lease/binding and owner-turn
permission. That does not currently inspect a C2 deterministic-input hold or
prove that an asynchronous request was built from the same semantic inputs as a
future wake claim. The future claim interface must carry/revalidate those inputs;
snapshotting whatever binding is current at launch is not that proof.

The model observer separately performs terminal-workflow, capacity and bounded
purpose checks before `charge_admitted_model`. Storage-aware builder calls do
not substitute for these checks. Every separately built reasoner must retain the
workflow audit through `bind_builder` or `bind_reasoner`. The initial receipt
correlation change in #1956 remains a prerequisite; it does not itself add a C2
hold or return failed reviewer results.

Two boundaries must stay explicit in the complete implementation map:

- The legacy multi-repository approval-broker lane (`process_one_repo`, near
  line 10565) deliberately uses `build_reasoner` without an Autoship
  `WorkflowGuard`. It cannot be presented as a guarded C2 retry or used to evade
  a preserved workflow's hold/accounting. Its existing approval-broker policy
  is distinct; any future convergence must preserve both authorities.
- Calibration and native acceptance/probe paths are diagnostic/observational
  entry points with their own workflow and immutable call reservations. Their
  model calls are not production wake evidence. Test-only calls and the
  `Github::call` transport methods are not model dispatches.

Remaining R/S handoff work is to bind exact trusted pre-dispatch key producers
and legacy-unknown behavior to every row, specify the early read-only decision
and final claim consumption transaction together with R/P, and test actual call
counts through these entry points. This table does not claim those interfaces
or their crash/race tests are complete.
