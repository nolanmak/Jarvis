# Native provider holds (#1706)

After provider preflight succeeds, a provider can become quota-blocked or the
eligible chain can become unavailable before the guarded build call. Native
fallback facts now persist distinct workflow waits at those controller boundaries:

| Lane fact | Reason | Action | Outcome |
| --- | --- | --- | --- |
| All eligible providers latched | `provider_quota_wait` | `await_quota` | `QuotaWait` |
| No eligible provider | `provider_configuration_unavailable` | `await_provider_configuration` | `ConfigurationFault` |

The hold preserves the current stage, evidence, candidate binding and compute
counters. It suspends the active inactivity clock without manufacturing progress.
Owner pause continues to override the public outcome as `DependencyWait`.

Quota eligibility uses the earliest reset in the affected lane's authoritative
latch report. Unknown reset times get an unpaid five-minute observation; no reset
is inferred from another provider or preset. Configuration waits also get a
five-minute observation. The original provider checks and cooldown latch still
control every productive dispatch. A checkpoint cannot clear a latch or grant
model/runtime authority. Existing unpaid activity observation remains distinct
from eligibility to execute.

Native-controller tests use temporary Git repositories and the real WorkflowGuard
and store. They persist/reopen both hold types, compare activity with runtime
events, verify owner-pause precedence and zero model calls, and preserve build
stage/evidence/binding while retaining the earliest known lane reset. These are
component tests, not production issue-to-merge acceptance.

This slice does not classify every provider failure, change real compute charging,
remove resource limits, add immediate provider-change notifications, close the broader fairness/custody requirements, or close #1706.
