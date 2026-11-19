# Yolo threads

A yolo thread is a Discord thread whose turns are answered by a hosted,
unfiltered model (by default Experiential Labs `glm-5.3-uncensored`) instead
of the normal assistant. It can read the owner's data and work in a scratch
workspace, and nothing that happens in it is carried into any other
conversation.

Tracking: epic #2200, spec #2243.

## Using it

- `/yolo` in the query channel opens a new thread and posts what the thread
  can do. Every message you send in that thread is answered by the yolo
  model, and every reply starts with a tag line such as
  `[yolo · glm-5.3-uncensored]`.
- `/yolo status` shows whether yolo is ready and what it has.
- A yolo thread is permanent. There is no "off": to talk to the normal
  assistant, go back to the parent channel or a DM. `/yolo off` in the thread
  says so.
- `/yolo` in a DM or in another thread points you at the query channel.
- In a yolo thread only `/yolo`, `/search` and plain messages are handled.
  Other owner commands (`/model`, `loop`, `!journal`, phone and autoship
  commands) are refused there.
- Attachments are not available in a yolo thread.

## What a yolo turn can and cannot do

Can:

- read the wiki and, when configured, the meeting transcripts;
- search past conversations, messages, notes and memory (read-only);
- run a fixed list of read-only lookups: Gmail search and attachment lists,
  finance status, transactions and summary, calendar events, Meetup events,
  recent LinkedIn DMs, repository documents, and issue list and view;
- read and write files in its own workspace,
  `<state dir>/yolo-workspaces/<thread id>/`, which persists across turns;
- on Linux, run a short list of programs there (`python3`, `node`, `jq`,
  `rg`, `sed` and the usual file tools; not `git`). Each call is one program
  with its arguments and no shell. They run in the kernel sandbox with no
  network, can write only in the workspace, and get a three-variable
  environment;
- search and fetch the web, only when `AUGMENTAGENT_YOLO_WEB=1` and a web
  provider is configured (see [WEB-PROVIDERS.md](WEB-PROVIDERS.md)).

Cannot:

- send, post, schedule, sign, upload or change anything outside its
  workspace: no email or message sends, no calendar changes, no loops, no
  issue creation, no provider pin changes;
- write to the wiki or to memory;
- be answered by, or fall back to, any other model. If the yolo model fails,
  the reply says so.

## How the thread is kept apart

- A turn that is not a yolo turn never runs in a yolo thread, whatever
  started it (voice, phone, the iMessage owner surface, a slash command).
- A yolo thread's messages are history for yolo turns only.
- Messages from a yolo thread are never stored in the searchable message
  store and never mirrored to iMessage, and its answers are never spoken.
- The registry of yolo threads is `~/.config/augmentagent/yolo-threads.json`
  (`AUGMENTAGENT_YOLO_THREADS_CONFIG` overrides the path). Entries are never
  removed. If the file exists but cannot be read, every thread under the
  query channel is paused until it is fixed; DMs and the query channel itself
  keep working.

Yolo tool calls are still written to the tool audit log, and so appear on the
dashboard.

## Configuration

| Setting | Default | Meaning |
|---|---|---|
| `AUGMENTAGENT_MODEL_YOLO_ENABLED` | unset | Yolo runs only when this is exactly `1`. |
| `EXPLABS_API_KEY` | none | Vendor key, from the credential store or the daemon environment. `EXPERIENTIAL_API_KEY` is also accepted. |
| `AUGMENTAGENT_YOLO_BASE_URL` | `https://api.experientiallabs.ai/v1` | Must be `https`, path `/v1`, and its `host:port` must be in the allowed list. |
| `AUGMENTAGENT_YOLO_ALLOWED_HOSTS` | `api.experientiallabs.ai:443` | Comma-separated `host:port`. |
| `AUGMENTAGENT_YOLO_MODEL` | `glm-5.3-uncensored` | Model id sent to the vendor and shown in the tag. |
| `AUGMENTAGENT_YOLO_ZDR` | on | Sends the vendor's zero-data-retention header. Set to `0` to stop. |
| `AUGMENTAGENT_YOLO_WEB` | off | Set to `1` to give yolo turns web search and fetch. |

`augmentagent doctor` reports the `yolo_profile` finding: `disabled`,
`ready`, or the one thing that is missing.

`augmentagent reasoner-selftest --profile yolo [--tool-probe]` runs one call
against the vendor. It needs the enable flag and a key like any yolo turn.

## What leaves the machine

Everything the yolo model reads goes to the model vendor as part of its
context: wiki pages, search results from memory, email and finance lookups.
The zero-data-retention header is a request. On a vendor plan that does not
include it, the vendor refuses requests that carry the header, and every yolo
turn fails until the plan changes or `AUGMENTAGENT_YOLO_ZDR=0` is set; with
it off, the vendor's default prompt capture applies.

With web tools on, the model's search queries and fetched URLs go to the web
provider that serves the call, and may contain private data the model chose
to search for.

The vendor moderates this model and can suspend uncensored access for the
whole account; Jarvis reports the vendor's error.
