# GPT-Live voice selection (#1622)

## Research decision (2026-10-27)

Keep the existing GPT-Live transport and select voices at session creation.
OpenAI requires a new session to change voice. The shortlist is:

| Config value | Documented character | First audio in one synthetic trial |
|---|---|---|
| `gleam` (default) | North American feminine, Natural | 1.756 s |
| `willow` | Irish feminine, Natural | 1.609 s |
| `quartz` | Australian feminine, Generated | 1.660 s |
| `delta` | Southern US feminine, Generated | 1.641 s |
| project `voice_…` ID | Original adult feminine American voice from description | 1.450 s |

The custom audition targets an adult woman in her mid twenties: bright, clear,
warm, relaxed American English. It is an original voice, not an imitation.
Provider metadata confirmed every selection. Each trial used the same synthetic
text, 8 kHz PCMU, silence input and no tool execution or PSTN call. Timing includes
connection startup. These are single observations, not a percentile benchmark or
proof of phone latency. Private audio previews and receipts are retained on the
operator host; custom IDs must not be committed. Naturalness and perceived age
require listening; neither is established by these measurements. The presets
have no documented age labels. The default remains Gleam until explicitly changed.

GPT-Live costs $0.05/min, billed per second. Prompt voice creation is free;
using it incurs normal session billing. These five trials reported 62 session
seconds in total (about $0.052), excluding unrelated usage. Telephony and task
reasoning are separate costs.

Alternatives: the existing ConversationRelay route supports ElevenLabs and
Amazon Polly voices. Those offer conventional text-to-speech selection, but
require the separate STT/reasoning/TTS pipeline; their TTS latency cannot be
compared directly with full conversational session startup. They were surveyed,
not benchmarked in this audition. Retaining GPT-Live avoids changing that pipeline
solely to select a voice. Relay settings remain independent.

Sources:
- [OpenAI Live voices and session selection](https://developers.openai.com/api/docs/guides/live-conversations)
- [Original prompt-created voices](https://developers.openai.com/api/docs/guides/custom-voices)
- [GPT-Live model pricing](https://developers.openai.com/api/docs/models/gpt-live-1)
- [Twilio ConversationRelay TTS voices](https://www.twilio.com/docs/voice/conversationrelay/tts-voices)
- [Twilio conversational pricing](https://www.twilio.com/en-us/products/conversational-ai/pricing)

## Switching without code changes

For the OpenAI Live backend, edit the operator `.env`:

```dotenv
AUGMENTAGENT_PHONE_LIVE_VOICE=willow
```

Accepted presets are `gleam`, `willow`, `quartz`, and `delta`. Unset preserves
Gleam. For an existing original prompt-created voice in the same OpenAI project:

```dotenv
AUGMENTAGENT_PHONE_LIVE_VOICE=voice_YOUR_PROJECT_ID
AUGMENTAGENT_PHONE_LIVE_VOICE_DESCRIPTION="Your original voice description"
```

The custom description is required, nonempty, at most 2000 UTF-8 bytes and
contains no control characters. Remove the description when choosing a preset.
It is appended to the session instructions. IDs must be `voice_` plus ASCII
letters, digits, underscores or hyphens, at most 256 bytes total. Unknown presets
or malformed configuration fail before dialing; there is no silent fallback.
Provider rejection or a different confirmed voice also fails preflight.

Settings are loaded when the daemon starts. Apply through the normal guarded
configuration rollout when idle; changing `.env` alone does not hot-reload the
running daemon. The next call uses the selection in both preflight and its actual
session. Existing sessions hold their original selection. There is no mid-call
switch, random alternation, or chat command in this version. Health exposes the
preset name or `custom`, never the custom ID or description. Clear both settings
and reload to restore Gleam. No new API key or telephony provider is required.

Automated tests validate configuration, session creation/confirmation, and local
WebSocket probe/call paths. A real owner call must still confirm that the selected
voice is audible and preferred; synthetic provider success is not that check.
