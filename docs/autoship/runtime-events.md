# Durable workflow events

`WorkflowStore::latest_workflow_events(now_ms)` returns
`Result<(Vec<augmentagent_channel_core::autoship_workflow_contract::WorkflowEvent>, bool)>`.
The vector contains the latest persisted event for each nonterminal shipping workflow,
ordered by workflow ID. Observation/calibration workflows are excluded.

The boolean is inventory completeness: an initialized empty store returns `([], true)`;
a legacy store without the event journal, or an active workflow missing an event,
returns incomplete inventory. Consumers must distinguish incomplete coverage from
healthy idle. Database or contract decoding failures return an error.

Native workflow events and owner-control changes persist the shared event in the
same transaction as the state change. Events include the current stage, typed outcome,
reason code, next action, cumulative actual compute usage, and meaningful-progress
time. Actor `controller` and origin `reconciliation` identify this ledger projection;
they do not assert that an original request came from CLI or Discord.

A first successful verification gate is meaningful progress. Repeated identical gate
facts, heartbeats, stage metadata and usage accounting cannot renew that timestamp.
The emitter also recognizes protected native-tool and guarded-step completion events
when those producers are installed. Missing historical progress is left at zero;
legacy stage/heartbeat timestamps are not backfilled as proof of work.

Reads work on read-only connections and never append events, backfill records, or
advance time. `now_ms` is retained in the interface for consumers but does not change
persisted facts. Merged and retired workflows are excluded from the active inventory.
This API does not change admission, scheduling, cancellation or merge authority.
