# Slack live two-way voice: feasibility record

Part of #1298 (epic #1281). Discord reference: #1220, [discord-voice/](discord-voice/).

Checked: 2026-09-29
Recorded status: `Unproven`

`Recorded status` is the status of the `LiveVoice` row in
`crates/augmentagent-channel-slack/src/surface.rs`. The test
`live_voice_stays_an_open_blocker_backed_by_the_dated_feasibility_record` in
`tests/surface_contract.rs` fails if the two disagree, if either voice row
stops citing this file, or if either row leaves `Unproven`.

## Conclusion

**No route exists today that lets a Slack app exchange live audio with the owner
inside Slack.** Slack does not document any API that lets an app or bot join a
huddle or send or receive huddle audio. The Calls API registers a call that
runs on someone else's infrastructure. Slack shows the call card and a Join
button, but the audio never passes through Slack.

The only documented route that carries two-way audio and can be started from
Slack is option C: a call registered through the Calls API, where Join opens a
voice room hosted by the daemon. The owner would talk in a browser or another
app, not in Slack. Whether that counts as "live voice in Slack" is a change to
the requirement, and the owner has to decide it (#1298: "changing the
requirement needs an explicit owner decision"). Until the owner decides that
and a real owner session has exchanged audio, live voice stays an open
`Unproven` parity blocker on #1298 and #1281. Voice clips (#1297) do not count.

No transport spike was written. The only route that carries audio (C) is a
browser voice gateway, not a Slack transport, and building it before the owner
decides would be speculative. The Slack side of C (`calls.add`) only registers
metadata. A spike of that part would prove nothing about audio.

## Method and limits of this record

- Sources: the Slack developer docs, Slack Help Center, Slack and Salesforce
  terms, Apple and MDN docs, and two third-party pages. Each was fetched on
  2026-09-29 and is cited below. `api.slack.com/apis/calls` now returns 302 to
  `docs.slack.dev/apis/web-api/using-the-calls-api`, so the `docs.slack.dev`
  URLs are cited.
- **Nothing was run against a Slack workspace.** No test workspace or token
  was used in this session, so every Slack claim rests on documentation only.
  Where the documentation is silent, the claim is marked **unverified**.
- **Absence claims** ("no API lets an app join a huddle") mean that nothing was
  found in the documentation surfaces listed in the Sources section. They do
  not prove the capability can never exist. Slack could add it later, and this
  record should be re-checked before release.
- No host runs were done on Linux or macOS. The host notes below are drawn
  from the cited Apple docs and this repo's service model: per-user
  LaunchAgents on macOS ([MACOS-SIDECARS.md](MACOS-SIDECARS.md)) and systemd
  user units on Linux.

## Evidence

| # | Claim | Source (checked 2026-09-29) |
|---|-------|-----------------------------|
| E1 | "It's important to know that Slack doesn't make the call." Slack shows a registered call "natively, with lists of participants, a join button, and metadata". The page does not mention huddles. | https://docs.slack.dev/apis/web-api/using-the-calls-api |
| E2 | `calls.add` requires `external_unique_id` and `join_url` ("The URL required for a client to join the Call"). Optional `desktop_app_join_url`: "When supplied, available Slack clients will attempt to directly launch the 3rd-party Call with this URL." Scope `calls:write`, bot or user token, rate tier 3. | https://docs.slack.dev/reference/methods/calls.add |
| E3 | `calls:write` is "Start and manage calls in a workspace". It covers `calls.add`, `calls.end`, `calls.participants.add`, `calls.participants.remove` and `calls.update`. The scope page states no plan-tier or review restriction. | https://docs.slack.dev/reference/scopes/calls.write |
| E4 | To post a registered call, use `chat.postMessage` with a block of `"type": "call"` and the `call_id`. The page does not say which URL a client opens on Join, and says nothing about plan tier, review or mobile clients. | https://docs.slack.dev/apis/web-api/using-the-calls-api |
| E5 | `user_huddle_changed` ("A member's data has changed") carries user profile fields, including `huddle_state`, `huddle_state_expiration_ts` and `huddle_state_call_id`. Scope `users:read`. It carries no audio and no way to join. | https://docs.slack.dev/reference/events/user_huddle_changed |
| E6 | Huddles are "Available on all plans". "On the free plan, huddles can have a maximum of two participants." Only "members and guests" are named as users. Apps and bots are not mentioned as participants. | https://slack.com/help/articles/4402059015315-Use-huddles-in-Slack |
| E7 | Huddle media goes to Amazon Chime infrastructure: "Approve *.chime.aws or IP range 99.77.128.0/18"; "UDP/3478 or UDP/22466, and TCP/443". The page does not mention apps, bots or APIs. | https://slack.com/help/articles/36284146785427-Guide-to-network-and-system-configuration-for-Slack-huddles |
| E8 | Third-party calling apps: "Owners and admins can set additional third-party calling apps to allow members to quickly start a third-party call from Slack." Some listed apps need a paid plan. How Join behaves is not described. | https://slack.com/help/articles/208492868-Voice-video-and-screen-sharing-apps |
| E9 | Slack API Terms (effective October 10, 2025): "you will not: … (B) access our APIs in any manner that (i) compromises, breaks or circumvents any of our technical processes or security measures associated with the Services …" and "(D) attempt to reverse engineer …". "You must use the APIs only in accordance with this Contract and the Slack API documentation." | https://slack.com/terms-of-service/api |
| E10 | Salesforce Acceptable Use Policy (last updated July 08, 2025; Slack links to it from slack.com/acceptable-use-policy), §5.A.XI: customers may not "Impersonate another person, entity, or Salesforce … or otherwise misrepresent themselves or the source of any communication". No clause found that addresses automating a user account in the Slack client. | https://slack.com/acceptable-use-policy → https://www.salesforce.com/content/dam/web/en_us/www/documents/legal/Agreements/policies/ExternalFacing_Services_Policy.pdf |
| E11 | A third-party huddle bot, `claw-huddle`: "Headless Chrome (via Puppeteer) logs into Slack using cookies and joins a huddle." It uses a real user's session cookie, not a bot token, and needs Linux with PipeWire virtual sink and source, FFmpeg and Node. The README has no statement about Slack's terms. | https://github.com/jlgrimes/claw-huddle |
| E12 | Recall.ai's Slack huddles product: "Recording happens on your user's device and captures their audio and video". The page describes receive and record only, with no way to speak into a huddle. | https://www.recall.ai/product/slack-huddles-api |
| E13 | Browser microphone access: `getUserMedia()` is available only in secure contexts ("a page loaded using HTTPS or the `file:///` URL scheme, or a page loaded from `localhost`") and "must always get user permission before opening any media gathering input". | https://developer.mozilla.org/en-US/docs/Web/API/MediaDevices/getUserMedia |
| E14 | NAT traversal: "If a host is located behind a NAT, it can be impossible for that host to communicate directly with other hosts (peers) in certain situations", in which case a relay (TURN) is needed. | https://www.rfc-editor.org/rfc/rfc8656 |
| E15 | macOS local network privacy (TN3179): launchd daemons, root processes and Terminal/SSH tools get local network access automatically. "The exception for `launchd` daemons doesn't apply to `launchd` agents." "Outgoing traffic to a local network address requires local network access". Traffic to non-local addresses is not covered. | https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy |
| E16 | macOS application firewall: when an app not in the list tries to receive incoming connections, "an alert message appears asking if you want to allow or deny the connection over the network or internet". | https://support.apple.com/en-ca/HT201642 |

## Options

| Option | Two-way audio with the owner? | In Slack? | Verdict |
|--------|------------------------------|-----------|---------|
| A. App or bot joins a huddle through a Slack API | No such API documented (E1, E5, E6, E7) | n/a | **Not available** |
| B. Huddle events (`user_huddle_changed`) | No: presence metadata only (E5) | n/a | **Not a route** |
| C. Calls API card that opens a voice room hosted by the daemon | Yes, over the daemon's own media path (E1, E2) | Controls, card and transcripts in Slack; **audio outside Slack** | **Technically viable; blocked on an owner decision** |
| D. Headless browser logs in as a user account and joins a huddle | Yes, in the huddle (E11) | Yes | **Rejected**: undocumented, uses a user account, policy unverified, cannot be tested offline |
| E. Recording SDK on the owner's device | Receive only (E12) | Partly | **Rejected**: cannot speak; third-party vendor |
| F. Voice clips (#1297) | Not live | Yes | **Does not satisfy #1298** (by rule) |

### A. A Slack app joins a huddle

- **Requires:** an API that does not exist in the documentation checked. The
  Calls API does not mention huddles and says Slack "doesn't make the call"
  (E1). The only huddle signal for apps is a profile-change event (E5). The
  huddle help and network pages name members and guests as users and Amazon
  Chime as the media path. Neither mentions apps (E6, E7).
- **Verdict:** not available. An absence claim; see Method.

### B. Huddle events

- `user_huddle_changed` tells an app that a user's huddle state changed and
  gives a `huddle_state_call_id` (E5). It carries no audio and no join
  mechanism. Per the issue, events alone must not be taken as capability.
- **Verdict:** useful at most for status ("owner is in a huddle"). Not a voice
  route.

### C. A registered Calls API call that opens a daemon-hosted voice room

What it is: the daemon calls `calls.add` with a `join_url` that points to a
voice room the daemon hosts or brokers (WebRTC in a browser, or a desktop app
through `desktop_app_join_url`). It then posts a `call` block in the owner's
DM or thread (E2, E4). The owner clicks Join in Slack and talks in a browser
tab. Speech-to-text, the native conversation and text-to-speech run as they do
for Discord (#1220).

- **Slack requirements:** a bot token with `calls:write` (E2, E3), plus
  `chat:write` to post the block. The documentation checked states no plan
  tier or app review for the Calls API (E3, E4), but the absence of a
  restriction is **unverified**. Workspace admins can pick which third-party
  calling apps members can start calls from (E8). Whether a custom
  single-workspace app needs that setting to post a call card is
  **unverified**.
- **Media path:** Slack carries none of the audio (E1). The owner's browser
  needs an HTTPS page (a secure context) before it can use the microphone
  (E13). Media has to reach the daemon:
  - The daemon is outbound-only on a laptop behind NAT, so a direct browser to
    daemon path is not reliable. A relay reachable by both sides (a TURN
    server or a hosted media server) is needed when direct connectivity
    fails (E14). Which relay to use, and whether to self-host it or buy a
    service, is an open design choice. No product has been evaluated here.
  - The alternative is to expose a public HTTPS and media endpoint on the
    host, which the laptop case rules out without a tunnel. Tunnel options
    were not evaluated.
- **Owner experience:** start, stop, status, interrupt and provider controls,
  and transcripts mirrored into the thread, can all live in Slack through the
  existing command and delivery paths. **The conversation itself happens
  outside Slack**, in a browser window or another app. What each Slack client
  does on Join (desktop, web, mobile) is **unverified**. The docs say only
  that clients "will attempt to directly launch" the desktop URL when one is
  given (E2). Mobile behaviour is not documented (E4). A Calls API card adds
  nothing to the audio path that a plain link would not. It adds the native
  card, participant list and Join button.
- **Security:** `join_url` works as a bearer capability. It must be
  single-use, short-lived and bound to the owner's Slack identity and to the
  binding for this conversation. It must be revoked on stop, restart and
  hangup, matching the Discord grant model in
  [discord-voice/tool-contract.md](discord-voice/tool-contract.md). The web
  room must itself authenticate the owner. How to do that (for example Sign in
  with Slack) is **unverified** and not designed here. A public media endpoint
  is new attack surface that the text-only Slack transport (outbound Socket
  Mode) does not have.
- **macOS, the daemon side:** the daemon does not use the microphone, so it
  needs no microphone prompt. It runs as a per-user LaunchAgent, and the
  launchd-daemon exception to local network privacy does not apply to agents
  (E15). Traffic from the daemon to a LAN address, such as an owner browser
  on the same network, can therefore trigger a Local Network prompt that a
  headless agent cannot answer. Traffic through a relay on the internet is
  not covered by that rule (E15). If the macOS application firewall is on, a
  binary that listens for incoming connections can raise an allow or deny
  alert (E16). Relay-only, outbound media avoids both. Neither prompt has
  been observed on a real Mac. Both are **unverified** until #1255 or #1300
  run it.
- **macOS, the owner side:** the owner's browser asks for microphone
  permission (E13). That is an interactive prompt in the owner's session and
  is expected.
- **Linux:** a systemd user unit has no equivalent privacy prompts. Opening a
  listening port depends on the host firewall. Audio codec, resampling and
  STT/TTS dependencies would reuse the Discord sidecar stack (Node, FFmpeg),
  which already runs on Linux. Apple Silicon and Intel builds of that stack
  for macOS are tracked in #1255.
- **Verdict:** this route actually carries bidirectional audio with the owner,
  and it could be built and tested offline (fake relay, deterministic audio,
  fake STT/TTS, as the Discord sidecar is). But the audio is not in Slack. It
  is Discord-parity voice reached through a Slack card. It can only close
  #1298 if the owner explicitly accepts that as meeting the requirement. Not
  built here.

### D. A headless browser joins a huddle as a user account

- **What it takes (E11):** a real Slack user account (a second seat or the
  owner's own), authenticated by the web client's session cookie. Headless
  Chrome drives the huddle UI, virtual audio devices feed TTS in and capture
  audio out, and the setup is Linux with PipeWire. On a free plan the huddle
  would hold only the owner and that account (E6).
- **Policy:** no Slack API is used, so the API Terms' requirement to use the
  APIs "only in accordance with … the Slack API documentation" (E9) offers no
  cover. Whether driving the web client with a harvested session cookie counts
  as circumventing Slack's "technical processes or security measures" (E9) is
  **unverified**; no explicit clause was found either way (E10). An agent
  speaking through a human's account risks the AUP's impersonation and
  "misrepresent … the source of any communication" clause (E10).
- **Engineering:** it depends on Slack's undocumented web UI and a live
  session cookie. It cannot be tested offline against anything real. Any
  client change breaks it, and the cookie is a full-account credential that
  would have to sit in the daemon's store.
- **macOS:** no virtual audio stack comparable to PipeWire exists by default.
  Chrome's microphone access under macOS privacy controls, and any
  audio-driver install, would need prompts a headless LaunchAgent cannot
  answer (**unverified**; not tested).
- **Verdict:** rejected as a supported route.

### E. A recording SDK on the owner's device

- The vendor describes recording on the user's device, with no way to speak
  into the huddle (E12). It would also put a third-party vendor in the audio
  path.
- **Verdict:** rejected. It is one-way, so it is not live two-way voice.

### F. Voice clips (#1297)

These are asynchronous, not live. #1298 and #1281 state explicitly that they
do not satisfy live-voice parity.

## What the repo enforces

- `surface.rs`: the `Voice` shared capability row and the `LiveVoice`
  interaction row are both `Unproven`, tracked by #1298, and their `basis`
  cites this file and the check date. `parity_blockers()` lists both.
  `SlackInteraction::LiveVoice.require()` returns
  `UnsupportedCapability("live_voice")`.
- `tests/surface_contract.rs`, test
  `live_voice_stays_an_open_blocker_backed_by_the_dated_feasibility_record`:
  both rows must stay `Unproven`, cite this record and #1297, and appear as
  #1298 parity blockers. This record's `Recorded status:` must equal the
  `LiveVoice` row, and `Checked:` must be an ISO date. Flipping the row
  therefore needs a code change, a record change and a test change together.
- No CLI command prints the Slack capability table or `parity_blockers()`
  today, so the blocker is visible only through the code, this record and the
  test.

## Test plan for an implementation (not written yet)

The repo uses `#[ignore]` only for paid or live probes
(`crates/augmentagent-channel-core/tests/native_session_live.rs`), not for
pending behaviour. So the shared voice conformance scenarios from #1298's TDD
plan are listed here rather than checked in as ignored tests. Each must start
red against a fake transport:

1. **Authorized binding:** Join and start are bound to the owner's Slack
   identity, the conversation's native session, and one active generation.
2. **Unauthorized participant rejected:** a different Slack user, an expired
   or reused `join_url`, or a room token from another conversation gets zero
   audio frames and no turn.
3. **Audio round trip:** deterministic PCM or Opus fixtures go through the fake
   relay, the fake STT, a native turn, the fake TTS and back to the fake
   client.
4. **Transcript finalized once:** one committed utterance gives exactly one
   thread mirror and one native turn, including after an STT reconnect.
5. **Mixed text and voice turns:** thread messages and speech share one
   per-conversation queue and one native session.
6. **Barge-in:** owner speech, or Slack `interrupt`, invalidates queued and
   playing TTS within a bound, and late chunks are fenced.
7. **Provider failure:** STT or TTS 401, 403, 429 or 500, quota fallback, and
   exhausted retries give one visible failure and a stopped state.
8. **Teardown on hangup and restart:** `calls.end`, owner hangup, daemon
   restart and relay drop each revoke grants, close provider sockets, never
   auto-rejoin, and never replay a turn.

Then a separately authorized real owner run in a test workspace, on a Mac and
on Linux, recording latency and native session identity. A CI or mocked result
does not satisfy it.

## Follow-up work

Blocking:

1. **Owner decision (#1298):** does a Slack-launched call with the audio
   outside Slack (option C) satisfy live-voice parity? If not, #1298 and the
   #1281 live-voice criterion stay open until Slack documents app access to
   huddle audio. Re-check this record before each release.

Only if the owner accepts option C, as proposed issues under #1281:

2. **Test-workspace Calls API probe:** `calls.add`, a `call` block, and
   `calls.end` with a synthetic `join_url`. Record what Join opens on desktop,
   web and mobile, and any plan or admin gate (the unverified items in C).
3. **Voice room gateway:** browser WebRTC client, relay choice (TURN or hosted
   media server), and owner authentication for the room. Reuse the #1220
   STT/TTS, queue and barge-in stack behind a transport trait, with the
   conformance tests above run against a fake relay.
4. **Slack controls and mirroring:** voice start, stop, status, interrupt and
   provider from the Slack thread (#1292 commands). Transcripts and
   interruption notices go through #1294 delivery, bound to the thread's
   native conversation (#1288).
5. **Host services:** a launchd and systemd unit for the gateway, and
   confirmation on a real Mac that relay-only media raises no Local Network or
   firewall prompt (coordinate with #1255 and #1256).
6. **Real owner acceptance** (#1300): audio both ways, tools in the text
   conversation, controls from Slack, latency recorded. Only this can move the
   rows to `Supported`.
