# Issue #1168 verification

Linux host: Chrome 146.0.7680.153; codex-cli 0.155.1; worker model
`gpt-6-astra`, native Codex subscription authentication. Parent selection is
independent. Existing-session bridge: NewsletterBuddy PR #29, main `afc9294`.
No cookies, credentials, screenshots or private browser captures accompany
this report. Tests use temporary profiles; live QA uses owned tabs in the
existing signed-in Chrome.

## Reproducible checks

From the repository root:

```sh
AUGMENTAGENT_MODEL_ROUTER_CONFIG=/tmp/1168-no-router.json \
AUGMENTAGENT_MODEL_SELECTION_CONFIG=/tmp/1168-no-selection.json \
XDG_STATE_HOME=/tmp/1168-rust-state CARGO_INCREMENTAL=0 \
cargo test --workspace --quiet -j 2
python3 scripts/tests/codex_tool_bridge_test.py
bash scripts/tests/updater-rebuild-trigger.test.sh
bash scripts/tests/updater-stamp.test.sh
bash scripts/tests/updater-restart-hygiene.test.sh
cd sidecars/computer-use
npm ci
LIVE_COMPUTER_TEST=1 \
NEWSLETTER_COMPILED_ROOT=/path/to/NewsletterBuddy/dist/src npm test
```

The two configuration paths above must be absent: isolate tests from the
operator's private model-router settings. `LIVE_COMPUTER_TEST` requires the
native login and model access. `NEWSLETTER_COMPILED_ROOT` enables the actual
cross-repository bridge integration test, rather than silently substituting a
mock. Without those variables their two integration cases explicitly skip.
Chrome and OpenSSL are required for the deterministic browser/TLS tests.

## Acceptance evidence

| Criteria | Evidence |
| --- | --- |
| AC-1, AC-10: delegation and return | Actual `wiki ask --stdin` owner request automatically calls `computer_task`; live flight and Hacker News tasks. Status replay regression in `HandoffTests.test_computer_status_polls_remain_live_while_mutations_keep_replay_protection`. Running reports contain progress only; final findings follow cleanup. |
| AC-2: owner boundary | Rust `computer_tools_require_trusted_owner_and_bind_identity_outside_model_arguments`; service test rejects forged token and another owner; MCP rejects unknown identity arguments. |
| AC-3, AC-8: existing Chrome and shared lease | `browser.test.mjs` preserves unrelated tab; `shared-bridge.test.mjs` runs both real workers against one bridge, proves exclusion in both directions and one upstream connection. Real owner-input event pauses the adapter. |
| AC-4, AC-11: model and generality | `live.test.mjs` runs actual Astra through screenshot/tool turns on JS flight POST and event filter fixtures. General live lookup follows a Hacker News discussion. |
| AC-5: evidence | `evidence.test.mjs`: observed prices, requested route/dates, wrong-date evidence mixing, stale/invalid timestamps and invented prices. Browser worker verifies remaining visible flight controls and reports missing restrictions as unknown. |
| AC-6, AC-7: executor policy | Typed operations, actual element label/keyboard checks, exact reviewed search RPCs, denied purchase/GET mutation, private IP and reserved IPv6 tests. `network.test.mjs` makes a real TLS connection with pinned DNS and proves a rebinding answer cannot hit the second listener. Task HTML CSP disables workers, objects and popups; task frames disable WebRTC/WebTransport; task WebSockets are intercepted. |
| AC-9: recovery | Browser fixtures distinguish password login and CAPTCHA; service missing-bridge case returns `chrome_debugging_required`. Durable resume tests preserve authority, generation, budget and model. Live recovery/cancellation recorded below. |
| AC-10: durability and pending calls | `tasks.test.mjs` covers duplicate/conflicting requests, restart fencing and cancellation; `runner.test.mjs` kills a pending model process within five seconds and removes private files. |
| AC-12: upgrade gates | `tasks.test.mjs` covers official/provider-corroborated discovery, ambiguity, failed evaluations, outages, promotion, pinning and previous model retention. `upgrade.mjs` executes deterministic suite plus actual candidate model fixture before promotion. |
| AC-13: artifact handling | Redaction test, owner-only state/socket permissions, inactive task expiry, no screenshot persistence; response headers/cookies stay inside forwarding transport. |
| AC-14: live workflows | Flight and non-flight owner CLI conversations, plus existing-session cancellation/recovery. See recorded runs below. |

## TDD record

Observed red tests preceded fixes for missing task modules, owner gating,
shared newsletter idle connection release, keyboard activation of a purchase,
cancel during Chrome connection, stale/invalid fare evidence, exact flight
search RPCs, retention, model discovery, real TLS address pinning, status poll
replay, sidecar deployment, login/CAPTCHA distinction, reserved IPv6, task
transport restrictions, and mixed-date price evidence. Red/green command logs
are retained privately under `/tmp/1168-*.log` on the development host; no
personal request bodies or credentials are committed as fixtures.

## Release limits

This release is browser research. It does not implement purchases, sending,
checkout or arbitrary desktop control. Ordinary HTTPS reads are permitted;
HTTP methods and button labels cannot establish all possible website
semantics. Exact POST RPC rules require maintenance when sites change.
Account-level safety is not inferred from a successful click.

Five-minute and 60-action limits are enforced outside the model. Native
transport token counts arrive at turn completion: usage is accounting, not a
hard cost ceiling. Recovery is an explicit fenced resume, rather than an
unbounded automatic retry loop. A stale cleanup lock deliberately blocks
subsequent tasks until the operator verifies cleanup. These limitations are
also documented in README.md.

NewsletterBuddy hosted checks could not start because that repository reported
an Actions billing block; its local checks passed (88 passed, 10 optional
skips; real Chrome bridge/worker integration 9 passed). Jarvis PR #1169 hosted
checks run normally and must be checked separately before merge.

## Recorded staging runs (2026-09-19 UTC)

- Full Rust workspace: 2,960 passed, 0 failed, 46 ignored (125 result groups).
  Final command exited 0. An intervening run during release compilation hit
  `hung_primary_times_out_and_fallback_serves` process-cleanup timing; the
  complete rerun after compilation passed. Release build also passed.
- Python bridge: 154 discovered, 146 passed, 8 host-dependent skips.
- Updater rebuild trigger/stamp/restart hygiene: 29 / 6 / 21 passed.
- Dashboard regression: 40 passed and production build passed.
- Existing-Chrome flight conversation, release binary, task started 17:42 UTC:
  ordinary request for one-way SFO→PHL, September 24, 2026, one adult economy.
  Parent delegated automatically, polled until completion, then returned
  $233 Frontier F9 2930/F9 2970, SFO 23:58 PDT September 24 → PHL 14:21 EDT
  September 25, DFW connection (4h11m), total 11h23m. Reported zero included
  carry-on/checked bags and unknown fare brand/personal item allowance.
  Comparison: Alaska $292, Southwest $342. Evidence observed around
  17:44 UTC on [Google Flights](https://www.google.com/travel/flights/search?tfs=CBwQAhoeEgoyMDI2LTA5LTI0agcIARIDU0ZPcgcIARIDUEhMQAFIAXABggELCP___________wGYAQI&tfu=EgoIAhAAGAAgAigB).
  Earlier live iterations caught replayed status reads, premature parent
  answers, unsupported fare labels and missing read RPC rules; this final
  staging result includes those fixes.
- Non-flight owner conversation at 17:36:43 UTC: browser opened HN newest,
  selected its first discussion with comments, followed the discussion, and
  returned observed title, 1 point, 2 comments and
  [discussion URL](https://news.ycombinator.com/item?id=49768461).
- Live recovery: started with deliberately unavailable bridge; observed
  `needs_action/chrome_debugging_required`; restored access by linking the
  selected existing bridge, resumed the same task/model, then cancelled while
  the native model was pending. Cancellation plus subsequent verification
  took 531 ms; no profile lock remained and status stayed cancelled.

Live fares are observations, not CI assertions or guaranteed availability.
Raw transcripts remain private on the host under `/tmp/1168-parent-*.log`.
Production-main verification is recorded in PR #1169 after deployment.
