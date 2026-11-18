# Family-budget observation contract, schema version 1

Part of #1703 / #1706, stacked on the native fence in #2139. This document describes the internal S read API implemented in `autoship_family_budget_observation.rs`. T owns authenticated command/funding intent, full readiness composition and activation; N owns private delivery; M owns shipping debits. None of these facts or actions creates authority or a new model call.

## API and units

`WorkflowStore::family_budget_observation(workflow, intent, incoming)` returns `Result<family_budget::Observation>`. SQL/I/O or unreadable-layout errors propagate as errors; callers must fail closed through S's bounded infrastructure/configuration hold handling, never replace an error with an empty Ready observation. That actual checkpoint/retry integration remains required before delegated activation. The same predicate runs inside the native charge transaction; final admission always revalidates current protected state. A hint is not an execution lease. `incoming` is the prospective admission count: a positive value checks that additional model work fits; zero is metadata-only intent refresh/inspection, with remaining usage still visible. This does not turn exhaustion into a prohibition on unpaid reconciliation.

The wire object carries `schema_version`, `workflow`, `intent`, `verified_model_admissions`, `committed_wall_ms`, `verified_tokens`, `token_completeness`, `per_member_reported_usage`, `limits`, `remaining` and `blockers`. Model and wall totals are independently nullable. Model admissions are the existing correlated controller admission units, not provider billing. Wall is existing committed controller elapsed usage, not a whole-run timeout or physical teardown proof. Current token completeness is always `unknown` and verified tokens are null; no complete token producer is claimed.

`limits` contains the supported explicit positive finite owner policy, if known. `remaining` is a separate derived status type with nullable model-admission and wall values; it is not a policy object. A known exhausted dimension has zero remaining. An unknown dimension stays null. Late actual costs may exceed the policy and remain visible rather than being clamped or discarded. No limits are invented from missing data or system-resource defaults.

## Blockers and action ownership

Every independently knowable blocker within the validated authority layout is returned. If required authority tables are missing, the result reports unknown authority rather than inferring other facts from an incomplete layout. Each blocker carries family/workflow references, typed `reason_code`, optional `metric`, actual committed `observed`, policy `limit`, prospective admission `requested`, and the existing shared workflow `NextAction` object as `next`. The metrics are `model_admissions` and `wall_ms`; no physical memory/disk quantity is mislabeled as one of them. Model `requested` is not charged usage. Wall has no prospective charge in this read check.

Blockers sort by owner pause, unknown authority, unknown funding scope, pending membership, missing policy, unsupported policy, unknown spend, then resource exhaustion. Within an exhaustion category, model admissions precede wall. References provide stable ordering within a category. Repeated observations with unchanged state return the same ordered facts and do not change checkpoints, progress, events, counters or merge allowance. With incomplete/missing membership, aggregate totals are unknown; reported known member counts remain available. An empty/partial SUM is never a verified zero family total.

| Reason | Next action code |
| --- | --- |
| `owner_paused` | `await_owner_resume` |
| `family_authority_unknown` | `await_authoritative_family_state` |
| `family_execution_scope_unknown` | `await_protected_execution_scope` |
| `family_membership_pending` | `await_verified_lineage` |
| `family_budget_policy_missing` | `await_owner_budget_policy` |
| `family_budget_policy_unsupported` | `await_compatible_budget_policy` |
| `family_spend_unknown` | `await_authoritative_usage_evidence` |
| `family_resource_exhausted` | `await_owner_budget_revision` |

These are internal machine-action codes, not claims that new public CLI recovery commands exist. `next.eligible_at_ms` is null: these actions depend on new authoritative input, not an automatic retry permission. T/S scheduling still owns bounded unpaid observation cadence and readiness-change handling.

## Composition and remaining delivery work

A budget-only Ready result does not prove publication lineage, predecessor merges, owner/provider authorization, current execution scope/custody or physical capacity. Existing independent work without a family keeps its existing rules; its empty family budget is not a fresh grant. Known independent child authority is verified separately from delegated family funding. The native fence remains the final atomic check before a new charge.

S's installed native accounting callback persists a `family_budget_wait` record with the generation, binding and original next action. Decoded-frame refusals retain schema version 1 with the budget observation. Early missing/malformed/unsupported-frame or missing-authority refusals use private wait schema version 2 with `context_reason` and a null observation: no funding intent or aggregate usage is fabricated. Existing version-1 budget records remain readable; the shared budget-read observation contract remains version 1. It preserves stage, evidence, actual charges and meaningful-progress time. The old controller is suspended before persistence, so storage failure cannot let it overwrite the hold or continue side effects; reporting requires a confirmed durable record. Run-local admission deltas remain independently billed even though the result is held. Repeating the same refusal creates no transition and renews no deadline. The checkpoint's five-minute eligibility value is an unpaid observation deadline; it does not replace the observation's null paid-action eligibility or grant another model call. Productive claims recheck a positive admission against protected budget state. Unchanged waits return no lease and do not increment attempts or create events, including after a free reconciliation rotates the generation. A trusted replacement funding frame can recover the saved prior next action when its positive budget check passes; displaying or activating a new policy alone does not rewrite the old intent. Native dispatch still requires its own fresh generation-bound frame. Ancestor pause and stale control revision after resume now use the same typed refusal path. Unpaid selection preserves that revision fence after free reconciliation, refreshes a changed blocking reason transactionally, and leaves attempts and meaningful progress unchanged. An unchanged observation creates no new transition. Unchanged unknown-context waits do not claim productive work and still permit free reconciliation. A trusted replacement frame can restore the saved next action; selection still grants no fresh native dispatch frame. Automatic T recovery publication and all delegated control/mutation fencing still require S/T integration before activation. T must consume the predicate with verified original-command intent in scheduling and selection, compose independent child limits/holds and lineage checks through the shared workflow service, and gate compatible readers before activating delegated work. Those S/T integrations remain required before live closure. T/N projections must retain their existing authenticated private-delivery boundary; they must not dump internal policy/funding references into public messages. Read-only output does not itself emit a public workflow event, claim successful execution, or debit M's allowance. Compatible production activation, authenticated policy production, lineage recovery and E live parent-to-child-to-merge acceptance remain unfinished dependencies; this slice does not close #1706/#1799.


## Exact wire shape and predicates

All optional fields below are present as JSON null when unknown; unknown numeric values are never represented by zero. All counts are unsigned integers. `ready()` is a Rust helper derived only from an empty `blockers` array; Ready is not a separately stored or serialized fact.

```text
Observation = {
  schema_version: 1, workflow: string, intent: Intent,
  verified_model_admissions: u64|null, committed_wall_ms: u64|null,
  verified_tokens: null, token_completeness: "unknown",
  per_member_reported_usage: [{workflow: string, model_admissions: u64, wall_ms: u64}],
  limits: Limits|null,
  remaining: {model_admissions: u64|null, wall_ms: u64|null}|null,
  blockers: [Blocker]
}
Intent = {kind: "independent", request_ref: string}
       | {kind: "delegated", family: string, manifest_ref: string,
          policy_ref: string, request_ref: string}
Limits = {schema_version: 1, max_model_admissions: u64, max_wall_ms: u64}
Blocker = {family: string|null, workflow: string, reason_code: Reason,
           metric: "model_admissions"|"wall_ms"|null,
           observed: u64|null, limit: u64|null, requested: u64|null,
           next: {action: string, eligible_at_ms: null}}
```

`Reason` is exactly the reason vocabulary in the table above. Owner policy limits must be positive and fit a signed SQLite integer; unknown policy fields or unsupported versions yield `family_budget_policy_unsupported`. Only T-authenticated explicit policy activation can set limits. `limits` describes the currently active supported policy, even when a supplied stale intent names an old policy and is therefore blocked; displaying it never refreshes that intent implicitly.

For a positive incoming count `n`, known model usage `u` fails if checked `u+n` overflows or exceeds the model limit; equality fits. Known committed wall usage fails if it is greater than or equal to its wall limit before the new admission. Both metric blockers are emitted when both fail, with actual `observed` usage and model `requested=n` (wall requested is null). Zero incoming performs these same authority/membership/policy/usage checks but emits no resource-exhaustion blocker: it requests no paid admission, may show zero remaining, and may be budget-Ready only for that metadata operation. Callers assessing a next model admission must pass its positive count. A Ready zero check cannot authorize a later positive call.

Remaining is `max(limit-used,0)` independently for each verified dimension. No policy means no remaining object; unknown model or wall usage leaves that dimension null. Non-metric blockers have null metric/observed/limit/requested. Only resource blockers populate those fields. Invalid or incomplete protected membership makes both aggregate dimensions unknown, even if a partial set has valid reported counters.

## Identity, replay and generation boundary

T supplies canonical lowercase repository identities. An issue workflow is `owner/repo#N`; a PR workflow is `owner/repo/pr/N`, with positive decimal N and no alternate spelling. The protected root plus two through six ordered unique children form a depth-one family. Root is position zero and children occupy positions one through the expected count. Reported entries retain that order; aggregate members and receipts are counted once. A parent root cannot become another family's child, and a child that is itself a family root cannot be staged recursively.

Verified model usage correlates each positive one-call `resource_charge` row with exactly one `workflow_event` of kind `native_model_admitted`: workflow identities match and charge `id` equals payload `receipt`. Duplicate native identity ownership, missing/orphan/malformed attribution, or model ledger/cumulative disagreement makes model usage unknown, not a guessed split of a legacy counter. Scoped native events additionally bind `budget_scope_ref` to the immutable stored generation's scope payload hash. Wall completeness independently requires committed wall rows to agree with each member's retained wall total. Reported member counters stay visible even if verified aggregate model evidence is unknown. A missing required member prevents either aggregate total from being certified.

`begin` binds the immutable original request/source/plan, expected child count and root. T commits it before publication. Staging can add verified exact identities only while pending; identical member replay is idempotent. `finalize` requires exactly the expected children, fixes one immutable manifest reference, and is idempotent only for that same reference. Changed membership/manifest/reference inputs reject; replay does not bypass the current root lease/binding check. Finalization alone creates no owner policy, grant or model call. T propagates any transaction error so `with_family_transaction` rolls back the whole admission batch; it must not catch an incompatible child error and commit partial authorization.

Execution intent is immutable within a lease generation. Policy activation uses an expected-predecessor compare-and-swap and never rewrites the old intent or spent charges. A stale policy, root-control revision, pause/resume or changed verified frame cannot expand that generation. After verified native quiescence, T uses normal fenced release/claim to obtain a fresh generation, rechecks current owner/repository authority and exact original funding/manifest/current policy, then calls `record_intent` before dispatch. Old intent/receipt history remains queryable. Root pause takes precedence for a known descendant; resume never clears an independent child hold. Budget observation itself does not verify possession of a current lease or frame, and cannot substitute for this transition.

Pending or uncertain publication has no implicit cancellation/deletion escape. T/#1799 owns authenticated lineage reconciliation, explicit owner adoption where provenance is missing, and any supported retirement/replacement transition. Until that producer exists and supplies compatible proof, the association stays incomplete with `await_verified_lineage`. Cancelling the original request cannot manufacture an independent child grant or erase charges. A changed plan needs separate explicit owner authorization and retained canonical spend, never automatic reslicing or a fresh counter. S/#1706 supplies decoded-frame and early-context callback hold production; remaining trusted recovery publication and control-path integration is still required; T/#1799 owns admission and readiness composition; E owns rollout/live proof. These are load-bearing unfinished integration exits, triggered before delegated activation. The carrying cost is retained versioned policy/manifest/scope references and original charge history, not a later inferred backfill.


## Fresh issue selection

Primary and configured-repository fresh intake read `WorkflowStore::family_wait_blocker` before selecting a waiting workflow. The read shares the productive claim's current/historical scope observation calculation and checks current ancestor control revisions. A still-blocked family wait yields to eligible peers, including after an unpaid reconciliation rotates the generation. The structured skip log includes the actual current blocker code.

Selection does not claim, register, migrate, charge, clear a wait or rewrite its checkpoint. An explicitly verified ready replacement frame rejoins selection; a displayed policy increase alone does not upgrade old intent. The productive claim and each native admission still revalidate authority, binding, budget and the required fresh-generation frame. Existing initial acceptance-hold inspection and missing/corrupt history handling remain separate.
