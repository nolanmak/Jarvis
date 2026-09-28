# Discord voice tool contract (version 1)

The native Codex and Claude turns load the `voice` stdio MCP server only while their owner-authorized text conversation has an active voice binding. The server exposes four tools:

| Tool | Arguments | Result |
| --- | --- | --- |
| `speak` | `{ "text": string, "utterance_id": string }` | `{ receipt, mirrored }` |
| `speech_status` | `{ "utterance_id": string }` | `{ receipt }` |
| `voice_status` | `{}` | `{ state, turnId }` |
| `voice_interrupt` | `{}` | `{ state: "interrupted" }` |

`text` is nonempty and at most 12,000 UTF-8 bytes. `utterance_id` is 1–64 ASCII letters, digits, `.`, `_`, or `-`. Reusing an ID with identical text returns the existing receipt and never sends a second `speak` frame to the sidecar. Reusing it with different text fails. The literal ID `final` means the tool delivered the final answer for this turn; the normal final answer is then mirrored as text but not automatically played a second time. A different ID may be used for speech while the agent is still working. A receipt is asynchronous: `queued` does not prove audible playback; `speech_status` can report `queued`, `playing`, `completed`, `interrupted`, or `failed`. `mirrored:false` means the text mirror could not be posted; retry the same ID and text to retry the mirror without replaying audio.

Sidecar receipts include `queuedAtMs` and, as stages occur, `ttsRequestedAtMs`, `ttsFirstByteAtMs`, `firstPlaybackAtMs`, and `stoppedAtMs` (Unix milliseconds). `firstPlaybackAtMs` records the Discord audio player's Playing transition; it is not proof that a remote listener heard a packet. `speech_status` is the way to retrieve later stages. The daemon logs the committed-transcript, native handler-dispatch, fully written native prompt, first nonempty native text-output, and answer-completion timestamps under the turn ID without logging raw audio or transcript text. The [latency-report procedure](setup.md#latency-report) joins those with independent speech-end observations for per-agent p50/p95; actual provider/Discord latency gates still require live measurements.

When speech is interrupted, the sidecar emits one `speech_interrupted` event per affected receipt. The daemon mirrors a notice to the bound text conversation; `partialAudioPlayed` says only whether the local player started before interruption, not whether Discord listeners heard audio.

The model cannot provide a guild, text channel, voice channel, generation, native session, or capability token. The daemon issues a random per-turn grant to the stdio MCP process via its private configuration. The MCP facade forwards one JSON line to an owner-only Unix socket:

```json
{"version":1,"grant":"<opaque per-turn grant>","method":"speak","arguments":{"text":"Hello","utterance_id":"final"}}
```

The control socket returns one JSON line containing `version:1`, `ok`, and either tool data or `error`. The daemon validates the grant and the active binding's exact guild, text conversation, and generation before every call. It revokes the grant when the turn ends; stopping or replacing the binding invalidates it earlier. The daemon mints the sidecar receipt ID as `<grant>:<utterance_id>` so a model-supplied ID cannot target another session. Both the socket directory and Claude's temporary MCP config are private to the daemon user; Codex receives the grant through the stdio server environment rather than CLI arguments. No Discord token or speech-provider key reaches the MCP process.

This tool contract is separate from the sidecar's version-1 Rust↔Node voice IPC. Neither contract contains raw audio. See [decision-record.md](decision-record.md) for evidence and release gates.
