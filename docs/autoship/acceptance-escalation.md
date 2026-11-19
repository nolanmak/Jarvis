# #1766 repair exhaustion: read-only production diagnosis

Source: `~/.local/state/augmentagent/autopr-workflow.sqlite3`, read over SSH using SQLite URI `mode=ro` and `PRAGMA query_only=ON`. No production builds, tests, calls, configuration, workflows, or service state were modified.

Workflow `nolanmak/jarvis#1766`, generation 4, original head `0e3d3128d1c267666153d701400dde5a1a50ceae`:

- Event 67117 checkpoints `acceptance_verifier_repair_exhausted` / `await_acceptance_input_change` at 2026-11-19 04:50:21 UTC.
- The retained original-verifier plan selected `autoship_whatsapp_coverage` as the black-box observation adapter with a JSON oracle `[false,true,false,false,false]`.
- Its line 88 called `team_coverage::find_coverage(&store, &row, ...)` with `row` from `oldest_pending_actions`, a tuple `(String, String, String, i64)`.
- Production `find_coverage` requires `&PendingActionRow`. Rust compilation failed with E0308 before the behavioral oracle ran.
- The last original-verifier result, key `d71c195ce04863a3efce323f7d7cdb42290078f3606640eddbe77691bd78b434`, retained `unexpected_failure`, including that compile error and output digest `b8336fde6d95f6b6079a85e3e90534f7b5e6ca90ceee2a78677f525a8f2b984d`.
- The last failure record, key `c8cf86a300c8038c9e764267644d3894a854f5ee8e569cf7fe8434aff8b406bc`, is slot 2 of the initial-repair lineage: the original candidate and two corrections exhausted the three-candidate budget. It still contains the tuple-versus-struct compile failure.

This is a generated verifier/adapter defect, not proof that WhatsApp coverage passed, failed its intended behavioral condition, or needed weaker acceptance. The controller preserved the evidence correctly but offered no automatically runnable strategy after the bounded correction lineage ended. The pre-existing `decompose_unverifiable` path was uncalled. #2267 wires that independent observable-child route and a bounded alternate-provider attempt into normal acquisition and dispatch, retaining the original requirements, candidate, evidence, charges and safety fences.

## Recovery policy (#2267, epic #2270)

An unchanged Scope hold for `acceptance_verifier_repair_exhausted`,
`acceptance_contract_invalid`, or `acceptance_failed` is eligible for planning
recovery automatically. Unpublished Build/Verify acceptance failures use the
same route. Existing PRs remain with their publication/review controller.

The controller retains the original checkpoint and bounded private diagnostic
references in an append-only `acceptance_strategy_started` event. It then uses
the existing observable-child planner, with failure evidence, every nonempty
original request line, frozen scoping criteria, and any retained registered
criteria. The planner must produce 2–6 ordered children whose bodies collectively
contain every criterion verbatim. The parent remains incomplete; each child must
pass the ordinary independent qualification/build/review/publication pipeline.

Each configured independent provider has its own durable completed-response
slots and decomposition receipt. A bounded completed-response rejection advances
to the other configured provider. Provider capacity, transient infrastructure,
unknown native outcomes, and uncertain GitHub publication do not consume a
completed strategy. Capacity and transient failures queue a five-minute retry;
unknown original outcomes retain the existing custody-recovery fence.

Only after every configured strategy has returned a completed invalid result
(or the request is already a bounded child) does the controller publish a fixed,
redacted issue summary. The original diagnostics stay private. The comment intent
and stable marker precede its RPC; retry scans for the exact body before posting
again. A lost response leaves reconciliation runnable. After a confirmed summary,
the parent waits for new execution inputs, preserving the original evidence and
charges. Nothing is inferred to have passed, and no owner requirement is removed.

Owner pause, live leases, checkout identity, original native custody, uncertain
publication operations, and family resource policy still fence dispatch. The
recovery grants planning authority only; it does not reset the original verifier
budget, change owner authority, approve a candidate, or perform deployment work.

## Rollout and observation

No environment setting, database edit, or manual retry command is required.
Normal deployment makes existing eligible holds selectable through the standard
picker and productive-claim path. Explicit owner/manual routing fences remain
in force. Production was inspected read-only during this work.

After deployment, compare the old reason counts with the new strategy events,
provider-capacity/retry waits, observable child publications, and any explicitly
summarized exhausted parents. A lower old-hold count alone is not proof of
end-to-end success. No post-deployment count reduction is claimed by this PR;
that receipt must come from the deployed controller and its real child outcomes.

Read-only pre-deployment inventory at 2026-11-19 06:03 UTC: 24
`acceptance_verifier_repair_exhausted`, three `acceptance_contract_invalid`, and
one `acceptance_failed` workflow. Four of the repair-exhausted rows are historical
PR workflows for #2135, #2224, #2241 and #2249; GitHub reports all four already
merged. Those rows stay with the existing model-free closed-target sweep, which
settles merged targets after its idle window. They must not generate new child
issues or reopen merged code. The live unpublished issue holds use the new
planning route. No production rows were edited to obtain this inventory.
