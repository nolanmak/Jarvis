# Restart custody for inactivity recovery

`AuditLogger::owned_run_inventory(owner)` reads the protected, anchored audit without
appending or sealing it. It returns inventory completeness, registered supervisor
count and quiescent supervisor count. These counts grant no release authority.
Unknown legacy/untracked admissions remain unknown; corrupt or incomplete audit
prefixes return an error.

Fenced native ownership starts a durable inventory. Provider and verification factories
register the supervisor before spawning it, including VM preflight. An owned logger
cannot be attached later to an unregistered process. If spawn fails before a process
exists, its registration is settled. Actual guards record completion only after their
existing cleanup checks. After controller death, retained private supervisor receipts
allow inspection even though Rust destructors never ran.

`recover_owned_runs(owner, successor_generation)` requires a newer generation. It
refuses live/incomplete supervisors and unresolved writer custody, verifies recorded
resource scopes against the kernel, and seals the old audit under the writer lock.
Old writers cannot append afterward. The resulting Rust cleanup capability identifies
the original owner and the exact successor generation. The sealed custody proof remains
durable if the new controller exits before committing workflow completion; reopening
it does not depend on temporary supervisor receipts still existing.

The store validates the current live lease when `complete_idle_recovery` is called.
Same-generation completion requires the live controller's proof. Completing an older
request requires its exact registered old audit and a sealed proof issued for the
current successor; a proof for a different successor cannot authorize the old lease.
The original request is settled once, only the original native owner is queued for
retirement, current checkpoint binding and cumulative usage are preserved, and lease
release remains a separate operation. Backoff follows the existing durable inactivity
failure history.

The caller must validate the successor lease before gathering proof and again when
committing it. Missing cleanup receipts or writer custody never become inferred
quiescence. Native controller renewal and unpaid maintenance now consume these APIs;
see [idle-recovery.md](idle-recovery.md). Legacy writer-custody recovery, coverage of
other controller roles and ready-peer scheduling remain part of #1706. Production
end-to-end autonomous merge acceptance remains separate.
