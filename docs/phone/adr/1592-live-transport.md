# GPT-Live phone transport

Status: implementation decision; account/live qualification pending. 2026-10-26.
Epic #1591; transport #1592. The target model is exactly `gpt-live-1`.

## Decision

Use the existing Rust phone listener to bridge Twilio bidirectional Media Streams
to the primary OpenAI Live WebSocket. Keep ConversationRelay as the production
default until #1596 passes. Reuse the existing Twilio dial reservation, destination,
owner binding, callback verification, call budget and single-consumer relay claim.
No new number, redial loop, phone verification ceremony or startup dial is added.
An owner-triggered readiness probe validates and finalizes a short Live session
before dialing; it carries no owner context and is never triggered by callbacks.
Its usage is persisted separately from the conversation in `phone_live_usage`.
Both attempts are registered before connecting; a crash leaves an incomplete
receipt with unknown usage. The final `session.closed` duration is recorded once,
including for a provider-reported failure. Socket loss never becomes zero cost.
Provider finalization, its reason, the local stop trigger and any finalization
failure remain separate fields; none implies the human owner hung up.

`phone metrics --last N` includes aggregate confirmed Live seconds and unknown
attempt counts for probes and conversations. The displayed USD calculation is
an estimate using the published $0.05/minute rate checked 2026-10-26, excludes
Twilio/backend charges, and leaves the total unknown if any attempt has missing
final usage. It prints no call/session IDs. Exact correlation remains in the
private store for the canary receipt. Actual invoice/cost reconciliation and
audible timing qualification remain in #1595/#1596.

This is an audio backend, not another Chat Completions fast provider. Backend
work remains in Jarvis's native session/approval harness and durable task queue.

## Alternatives and evidence

Twilio Agent Connect is the official integration. Its documented Python and
TypeScript SDKs include `GPTLiveProvider`, support outbound calls, and bridge the
same Media Streams protocol. The current tutorial demonstrates Responses
rather than Jarvis's required client delegation; client integration still needs
application work. Using TAC here would add a separately supervised runtime and
an authenticated IPC layer around existing Rust authorization and task state.
Choose the direct Rust bridge to keep those boundaries in one process. This is
not a claim that TAC cannot support client delegation or that the account lacks
TAC access. No TAC-only service/account dependency is introduced.

OpenAI recommends WebRTC even for server connections. Here Twilio already
provides an ordered WebSocket stream in G.711 mu-law. A WebRTC leg would add
an RTP/ICE stack, clock conversion and another media queue. The supported Live
WebSocket accepts the same codec at 8 kHz, so bytes can pass through unchanged.
SIP would instead require trunk/project routing and a different inbound call
correlation/control lifecycle; it offers no needed capability for this existing
outbound-only service. Revisit the choice if measured latency misses #1591.

## Wire and authority contract

* TwiML selects `<Connect><Stream>` only for an explicitly enabled Live call.
  The existing callback authorizes the persisted owner before connection.
* Verify `X-Twilio-Signature` against the configured public WSS URL, then match
  account SID, call SID and stream SID in `connected`/`start`. Persist a single
  relay claim before opening any provider connection or executing backend work.
* Require mono `audio/x-mulaw` at 8000 Hz. OpenAI uses
  `audio.format={"type":"audio/pcmu","rate":8000}` for both directions.
  Audio is base64 raw codec bytes without file headers.
* Accept ordered `media` frames only after startup; reject wrong identities,
  duplicate starts, malformed payloads, unsupported formats and reordered audio.
* Forward `media` followed by a unique `mark`. A returned mark confirms that
  preceding audio drained, not when its first sample became audible. A `clear`
  releases audio but returned marks for cleared buffers are not playback proof.
* Buffer at most two seconds (16,000 mu-law bytes) awaiting playback, with a
  separate bounded outstanding-mark count. Overflow fails the session explicitly;
  it never queues unlimited delayed speech or silently drops tool results.
* A `stop`, socket close or generic terminal call status does not establish an
  owner hangup. Keep provider failure, app stop and ambiguous closure separate.
* No raw audio/transcripts/provider error bodies enter public diagnostics.

## Qualification still required

Answered calls supply an initial developer message requesting a brief greeting;
the pre-dial readiness probe does not. The socket fixture verifies greeting
audio can reach Twilio before any inbound audio, but only a live call can prove
that the selected voice actually follows that instruction.

Interruption qualification must account for the Live protocol rather than
importing Realtime turn events: output audio has no timestamps or audio-done
event, and transcript fragments have no completed-turn event. OpenAI documents
local playback clearing and instruction steering, but an instruction acceptance
is not a safe-resume or playback acknowledgment. The remaining interruption
implementation must specify a bounded recovery policy and test late audio after
clearing, short backchannels, and longer corrections. Do not claim the prompt
alone guarantees immediate yielding or that a returned cleared mark was heard.

Fixtures pin documented message shapes with synthetic identities/audio; they
are not captures from this account and cannot establish live compatibility.
The signed HTTP/WSS integration tests must prove zero sessions/backend work on
unsigned, replayed or cross-owner input. The account must pass an explicit call
with the OpenAI project before production switches. Public media endpoint reach,
codec agreement, interruption behavior, measured audible latency and usage are
recorded in #1596. Missing OpenAI credentials block the canary, not offline work.

## Primary sources checked 2026-10-26

- [OpenAI partner integrations](https://developers.openai.com/api/docs/guides/live-partner-integrations)
- [OpenAI WebSocket audio formats and events](https://developers.openai.com/api/docs/guides/voice-websockets)
- [OpenAI Live conversation events and startup input](https://developers.openai.com/api/docs/guides/live-conversations)
- [OpenAI playback controls and interruption recovery](https://developers.openai.com/api/docs/guides/voice-server-controls?api=live#check-speech-before-playback)
- [OpenAI Live usage accounting](https://developers.openai.com/api/docs/guides/voice-latency-cost)
- [GPT-Live 1 published pricing](https://developers.openai.com/api/docs/models/gpt-live-1)
- [OpenAI telephony choices](https://developers.openai.com/api/docs/guides/voice-sip)
- [Twilio Agent Connect overview](https://www.twilio.com/docs/conversations/agent-connect/overview)
- [Twilio GPT-Live TypeScript integration](https://www.twilio.com/en-us/blog/developers/tutorials/integrations/tac-gpt-live-voice-ai-agent-typescript)
- [Twilio Media Streams messages](https://www.twilio.com/docs/voice/media-streams/websocket-messages)
