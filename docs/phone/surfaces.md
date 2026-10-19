# Phone entry-point audit

This is a source audit for AC08 of #1496, not a live qualification receipt.
The shared preset inventory is `docs/reasoner-capabilities.json`; its source
scanner detects additions to production preset constructors and wrappers.
The phone-specific audit must also follow callers and authentication, because
sharing `ask_opts` does not confer phone authority.

| Entry point | Current phone boundary | Evidence / outstanding work |
| --- | --- | --- |
| Discord owner conversation | Owner/control-conversation binding and explicit canonical owner link; phone context wraps the production query turn | `augmentagent-approval-discord/src/phone.rs`, CLI `discord_phone.rs`, `discord_phone_session_tests`; live origin evidence outstanding |
| Slack interactive owner conversation | Slack owner binding, control conversation and canonical owner link; harness scopes phone context | `augmentagent-channel-slack/src/harness.rs`, `interactive.rs`, CLI `slack_serve.rs`; interactive-surface tests; live origin evidence outstanding |
| WhatsApp authenticated self-chat | Device identity and self-message checks, stored owner/control binding and canonical owner link | `augmentagent-channel-whatsapp/src/control.rs`, CLI `whatsapp_phone.rs`, `whatsapp_phone_session_tests`; live origin evidence outstanding |
| Accepted phone call | Persisted originating conversation; acceptance and current authority checked before entering shared native-session runner | `augmentagent-channel-core/src/phone_agent.rs`; phone-agent/session tests; actual native tool/approval and audible qualification outstanding |
| Local `wiki ask --daemon` | Same-UID private Unix endpoint, explicit `cli` owner link, persisted local control conversation and shared native turn runner | CLI `local_phone.rs`, `local_phone_transport.rs`; signed loopback three-phone-turn test preserves native ID across local listener restart. Live qualification remains outstanding. Plain one-shot `wiki ask` does not attach phone tools |
| Scheduled `/loop` prompts | Uses `ask_opts`, without phone tool attachment | CLI `LoopReasonerRunner::run_prompt`; scheduled unsolicited calls are explicitly outside #1496. Do not infer phone permission from its owner ID |
| Telegram bot owner DM / subscribed messages | Subscription allowlist feeds triage/draft/approval, not the authenticated interactive phone harness | `augmentagent-channel-telegram-bot/src/channel.rs` calls `triage_opts` and `draft_opts`; no phone initiation claimed. Owner-DM delivery alone must not grant phone authority |
| iMessage | History ingestion and existing message delivery; no authenticated phone command adapter established | `docs/IMESSAGE.md`; authenticated control adapter is a dependency before claiming iMessage call initiation. Preserve sender and attachment ingestion |
| Deft | Intentionally not spawned by the daemon; protocol scaffold | CLI daemon startup explicitly leaves `DeftChannel` inactive. Its query-shaped types do not establish a production phone entry point |
| Imported mail/calendar/documents and other passive ingestion | Content may enter shared presets, but does not mint an authenticated phone context | All 13 shared presets start without phone authority. Per-channel/custom-constructor conformance audit remains open |

## Coverage and remaining inventory work

`phone_tool_configuration_requires_authenticated_context_and_preserves_other_tools`
checks every shared `reasoner.rs` preset against the source-backed manifest. It
checks both absence of default phone tools and no mutation/grant when attachment
is attempted outside authenticated context. Its authenticated case also proves
existing tools/settings survive attachment and revoked bindings fail.

This is necessary but not sufficient for AC08. Finish auditing the custom
constructors and option wrappers listed in `reasoner-capabilities.json`, and
trace every production call path that establishes owner authority. Add executable
conformance for those paths, including the new local daemon path above. Preserve the
explicit exclusion of passive ingestion and unsolicited scheduled calls; do not
solve missing coverage by granting those paths a default owner context.

The three mandatory chat adapters have offline initiation/status/stop coverage.
They still need live receipts, and this audit does not establish that every
integration can initiate a call. The local CLI requires `--daemon` and explicit operator setup; iMessage
initiation remains unsupported.

## Production wrapper composition regression

The Discord session fixture now installs a synthetic computer-tool entry point
and inspects the final production WikiQuerier options on both text and accepted
phone turns for both provider selections. Computer-use, model-switch and
Composio-auth MCP entries and permissions must survive phone attachment;
computer identity must remain bound to the current conversation/turn, approval
history access must remain available, and legacy voice permissions must be
absent. The fixture uses scripted providers and never executes the synthetic
computer entry point. It does not establish external-tool behavior or complete
custom-constructor conformance.

This regression initially failed because authenticated private Discord text
turns still depended on the legacy query-channel environment for approval
history. The shared native text runner now enters the existing owner-history
scope only after revalidating its private origin grant and owner revision,
matching accepted phone turns. The scope is enabled only when the origin equals
the stored direct conversation. Passive ingestion does not enter this runner.
The Discord and WhatsApp session regressions pass after this change (two tests,
both provider selections within each test).

`native_text_history_requires_registered_direct_conversation_and_current_owner`
adds the negative boundary checks for both scripted native providers: a direct
conversation receives history scope; an authorized control channel does not,
even inside an outer history-enabled scope; revocation rejects before another
provider call; and the task-local history scope does not escape the turn. All
four runtime tests pass locally, including the Slack adapter session test.
The required CI runner now expects at least four runtime tests.
