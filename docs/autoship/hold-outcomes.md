# Native hold outcomes (#1706)

Activity status and runtime events classify native checkpoint reason/action codes
through `autoship_outcome_classification.rs`. This removes the disagreement where
an invalid contract or unavailable verifier appeared as a generic dependency wait
in activity while the exported workflow event described its failure class.

| Native reason / action | Outcome |
| --- | --- |
| `acceptance_contract_invalid` / `await_acceptance_input_change` | `ConfigurationFault` |
| `readiness_assessment_invalid` / `await_readiness_input_change` | `ConfigurationFault` |
| `acceptance_verifier_infrastructure_unavailable` / `await_verifier_infrastructure_recovery` | `RetryableInfrastructure` |
| Owner pause | `DependencyWait` with `owner_paused` |

Classification describes the blocker. The existing next action, input-change
fence, eligibility, physical custody, owner controls and resource admission still
decide whether work can resume. An infrastructure outcome does not authorize a
new model call or bypass the original verifier recovery path. Holds retain their
suspended inactivity clock and bounded unpaid observation. No merge allowance or
compute accounting changes.

Native controller regressions acquire a real workflow in a temporary Git
repository, invoke `WorkflowGuard::hold_initial_acceptance`, finish, reopen the
store read-only, and compare persisted activity with exported events. They cover
both configuration reasons, infrastructure, owner-pause precedence and zero model
calls. These cases do not complete provider configuration wakeups, all code/review
failure producers, ready-peer scheduling, or live acceptance for #1706.
