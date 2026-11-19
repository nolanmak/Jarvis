# Conservative scope defaults and validator repair

Scope readiness assumes conservative, reversible implementation choices without
waiting for an owner response. Unknown external capabilities remain unavailable;
use the existing local path and leave optional integrations disabled. Assumptions
never authorize spend, new accounts, credentials, security/privacy exposure,
live external qualification, or irreversible production changes. Escalation is
reserved for choices where every alternative requires one of those effects.

The readiness response classifies each alternative with `decision`, `effects`,
`reversible`, `default_off`, and `new_account_dependency`. The controller chooses
the first safe alternative, records it in immutable workflow evidence, and posts
an **Assumed decision** issue comment before proceeding. A publication failure
stops the attempt. Retrying reconciles the stable comment body/marker before
posting again. Recorded assumptions accompany builder and acceptance guidance,
even when the spec model fails; original requirements remain intact.

Invalid completed contracts and readiness assessments get three durable model
attempts. Corrections receive the exact validator errors and prior output;
readiness citation errors also include already-observed literal source excerpts.
New correction calls back off 100ms then 200ms. Exhausted responses replay without
new admissions. Final errors stay in private workflow diagnostics. Provider
failures and missing original-verifier custody are not schema errors and cannot
be repaired by pretending that evidence exists.

On deployment, `safe-scope-repair-v1` makes legacy scope holds for
`material_external_decision_missing`, `acceptance_contract_invalid`, and
`readiness_assessment_invalid` eligible for one policy reconsideration without
editing the issue or advancing main. Admission requires the normal checkout,
lease, owner-control, and publication checks. It appends the original checkpoint
verbatim to an immutable `scope_policy_reconsidered` event. Old model responses
are preserved. A new scope namespace is used only when there is no original
qualification evidence after the last owner recovery. Otherwise original scope
and witness identities remain required, and only model-repair budgets change.

This is scheduling/model policy, not owner recovery or acceptance approval.
Paused/manual workflows, lost checkout continuity, real qualification custody
holds, and pending publication remain protected. New-policy exhaustion cannot
re-enroll itself on each tick. No operator database edits or blanket readoption
commands are needed.
