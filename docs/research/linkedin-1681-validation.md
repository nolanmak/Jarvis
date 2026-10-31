# LinkedIn archive acceptance evidence (#1681)

This map links the epic and child criteria to executable evidence. Test names
below refer to production parsing, persistence, dispatch, or routing code—not
live-mailbox completeness. The current command results and tested revisions are
recorded in [PR #1688](https://github.com/nolanmak/Jarvis/pull/1688).

Deployed/browser verification remains a separate delivery gate. Do not close
#1681 or its rollout child on the strength of these offline checks alone.

## Parent criteria

| Criterion | Evidence |
| --- | --- |
| E1: exact corpus, groups, owner direction, participants | [Corpus manifest][manifest]; `assert_corpus`, concurrent polling and process recovery in [archive_recovery][recovery]; production polling in [live_routing][routing]. |
| E2: replay, crash boundaries, concurrent poll/backfill | [archive_recovery][recovery] exits without destructors before each page operation and after each commit (108 exits for 54 pages), verifies IDs/bodies/source times/participants, and uses independent SQLite connections for concurrent poll/backfill. [archive_store][store-tests] verifies transaction rollback, overlap edits, and account collisions. |
| E3: bounded resumable coverage, errors and loops | Two-page restart test in [archive_recovery][recovery]; expiry, failed-thread, cancellation and retry tests in [archive_sync][sync]; atomic loop rollback in [archive_store][store-tests]; continuation tests in [archive_listing][listing]. |
| E4: index health, body/member/date queries | [archive_store][store-tests] verifies missing/stale/queued/FTS state and ambiguity; [archive_sync][sync] checks exact marker IDs; [archive_recovery][recovery] uses registered participant search and a 21-day filter that excludes old events imported today. |
| E5: full chronological Unicode reads and owner replies | Registered search-to-thread traversal in [archive_recovery][recovery], long-body/snapshot reconstruction in [archive_store][store-tests], and parsed live CLI reads in [CLI dispatch][cli-read]. |
| E6: 40 conversations, continuation, JSON contract | [archive_listing][listing], parsed CLI dispatch in [CLI dispatch][cli-read], and [command documentation][docs]. |
| E7: no archival automation; positive approved direct send | [live_routing][routing] spies on reasoner/card/notification/send calls; [send_guard][send] exercises actual outbound HTTP guards; [approval tests][approval] exercise positive send, concurrent claims, replay and stale group cards. |
| E8: typed finite failures, retry timing, cancellation | [archive_transport][transport] and [archive_sync][sync] cover HTTP/auth/redirect/shape/timeout/Retry-After, sanitized errors, injected-clock deadlines, and resumable cancellation. |
| E9: registered read-tool and permission path | Registered `search_messages` → `read_conversation_thread` in [archive_recovery][recovery]; `ask_mode_social_card_allowlist_tests` in [reasoner permissions][permissions] verify scoped read commands and absence of a broad write wildcard. |
| E10: deployed revision, complete scoped backfill, browser parity, replay and incremental cycle | **Pending managed rollout and private host/browser receipts.** See the required receipt fields below. |

## Child criteria

| Child | Evidence and limits |
| --- | --- |
| #1682 A1, A6 | HTTP corpus, exact request/ID/participant assertions: [archive_sync][sync], [archive_recovery][recovery], [manifest][manifest]. |
| #1682 A2 | Sender/owner/non-text parsing: [archive_transport][transport]. Opaque attachment/unavailable-body metadata survives storage/index/replay in [archive_events][events]. Synthetic metadata does not claim a verified provider deletion schema. |
| #1682 A3 | Listing overlap/change detection, identical-ID deduplication, conflicting-ID refusal, cursor expiry and transaction loop rollback: [listing][listing], [transport][transport], [sync][sync], [store tests][store-tests]. |
| #1682 A4, A5 | Typed failures and secret-safe diagnostics in [transport][transport]; durable account deferral, exact retry deadline, cancellation in [sync][sync]. Two-server redirect test rejects cross-origin forwarding. |
| #1683 B1 | Existing message/action preservation through two additive migrations, plus exact corpus: [recovery][recovery]. |
| #1683 B2, B3 | Process exits at every page boundary, concurrent independent importers, replay, edits, losing checkpoints and failed transaction rollback: [recovery][recovery], [store tests][store-tests]. |
| #1683 B4 | Two-page resumes, failed thread, auth/throttle/cancellation and bounded expired-cursor recovery: [recovery][recovery], [sync][sync], [transport][transport]. |
| #1683 B5 | Queue/index health and edits: [store tests][store-tests]. Non-text event metadata and absent bodies: [events][events]. Upstream-omitted events cannot be recovered by this importer. |
| #1683 B6 | Actual production polling and triage spies: [live routing][routing]; durable candidate/claim tests: [archive_live][live]. |
| #1683 B7 | Actual offline status binary reports per-thread state, direction/kind counts, source bounds, page progress and index backlog: [CLI integration][cli]. Discovery-only budget persists unvisited scopes: [sync][sync]. Dry-run bypasses disk-store bootstrap: [CLI integration][cli]. |
| #1684 C1 | Limits, overlap, caps, account-bound continuation: [listing][listing]; actual parsed command dispatcher: [CLI dispatch][cli-read]. |
| #1684 C2, C3 | Silent membership, same-name ambiguity, sender semantics, exact FTS IDs, source chronology and legacy dates: [store tests][store-tests], [sync][sync], [recovery][recovery]. |
| #1684 C4, C5 | Unicode continuation/new-arrival boundary and scoped missing/unverified/partial reads: [store tests][store-tests]; detailed offline status: [CLI integration][cli]; failed reads: [transport][transport], [sync][sync]. |
| #1684 C6 | Parsed live CLI argument dispatch: [CLI dispatch][cli-read]; actual status binary: [CLI integration][cli]; registered MCP workflow: [recovery][recovery]; scoped allowlist: [permissions][permissions]. |
| #1685 D1, D2 | Historical/group/owner negative paths and live direct burst with full context: [live routing][routing], [archive_live][live]. |
| #1685 D3, D5 | Fresh membership guards reject group, unknown, unowned, wrong-thread and forged direct responses before POST: [send_guard][send]. Persisted/reloaded direct-to-group card is rejected by the real approver: [approval tests][approval]. |
| #1685 D4 | Three-message direct burst produces one card and no send: [live routing][routing]. Real approved send is exactly once across concurrent connections and restart replay: [approval tests][approval]. |
| #1685 D6 | Sender/direction/source assertions: [sync][sync], [recovery][recovery], [CLI dispatch][cli-read]. Membership gates use authoritative participants, not a user-supplied label. |
| #1686 V1–V4 | Combined HTTP → transactions → index → parsed CLI / registered MCP evidence above; real status binary distinguishes partial threads and index backlog. Full-history scope is explicit; source-date query filtering does not change archival scope. |
| #1686 V5–V7 | **Pending deployed backfill, browser reconciliation, replay, and subsequent incremental cycle.** |
| #1686 V8 | This map plus PR command receipts; attach private/aggregate rollout evidence before closing the parent or children. Independent #1662 requirements remain separate. |

## Verification commands

Use `scripts/cargo-managed.sh` for local runs. The dedicated LinkedIn CI contract runs the
LinkedIn crate, recall libraries, CLI integration and parsed-dispatch/approval
tests, reasoner allowlist tests, and affected store/auth contracts on Linux and
macOS. It runs independently of the general platform suite, with the same
LinkedIn commands and no live credentials. CI inventory validation:
`python3 scripts/ci-contract.py verify`.

## Required host receipt

After implementation/CI gates pass, use the normal updater and
`augmentagent deploy snapshot` for rollback protection. Record:

- Running revision, observed category inventory/time, per-category coverage,
  mailbox/history page counts, conversation/message/direction/kind totals,
  source-date bounds, failures/retry times, and index health.
- Private comparison of at least three known groups and one owner-latest direct
  thread, including a conversation beyond page one and a multi-page history.
  Match participants, actual senders, ordering, bodies, and latest replies.
- Replay counts and absence of archival automation, followed by a subsequent
  deployed incremental cycle and its committed/indexed state.
- Every inaccessible or omitted event. Do not label unresolved omissions as
  complete coverage. Publish aggregates only; no names, bodies, cookies or raw
  captures in repository/PR receipts.

Cleanup of this task's worktree, temporary probes/logs and registered build cache
follows merge and verified deployment. Other sessions' resources are not owned
by this task.

[manifest]: ../../crates/augmentagent-channel-linkedin/tests/fixtures/archive_manifest.json
[recovery]: ../../crates/augmentagent-channel-linkedin/tests/archive_recovery.rs
[store-tests]: ../../crates/augmentagent-channel-linkedin/tests/archive_store.rs
[sync]: ../../crates/augmentagent-channel-linkedin/tests/archive_sync.rs
[listing]: ../../crates/augmentagent-channel-linkedin/tests/archive_listing.rs
[transport]: ../../crates/augmentagent-channel-linkedin/tests/archive_transport.rs
[events]: ../../crates/augmentagent-channel-linkedin/tests/archive_events.rs
[live]: ../../crates/augmentagent-channel-linkedin/tests/archive_live.rs
[routing]: ../../crates/augmentagent-channel-linkedin/tests/live_routing.rs
[send]: ../../crates/augmentagent-channel-linkedin/tests/send_guard.rs
[approval]: ../../crates/augmentagent-cli/src/linkedin_approval_tests.rs
[cli-read]: ../../crates/augmentagent-cli/src/linkedin_read_dispatch_tests.rs
[cli]: ../../crates/augmentagent-cli/tests/linkedin_archive_cli.rs
[permissions]: ../../crates/augmentagent-channel-core/src/reasoner.rs
[docs]: ../LINKEDIN.md
