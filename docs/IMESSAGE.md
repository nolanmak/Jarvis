# iMessage history sync and replies

The exporter, sender and scheduler ship in `scripts/imessage/`. You need one
source repository; message data lives in a private directory outside it. A Mac
with Messages synced to your account exports history. The agent can read that
directory locally or receive it over SSH on Linux. With sending enabled, the
agent drafts replies as approval cards and a sender on the Mac delivers the
ones you approve; see [Sending replies](#sending-replies).

## On the Mac

Use Python 3.9 or newer. The exporter uses Python's standard library. Run commands
from this checkout; you do not need to build or run the agent on the exporting Mac.

1. Open Messages and let your history finish downloading.
2. In System Settings → Privacy & Security → Full Disk Access, grant access to
   your terminal for the initial export. The scheduled Python executable also
   needs access; find its path with `python3 -c 'import sys; print(sys.executable)'`.
   macOS controls access to Messages data through [Full Disk Access](https://support.apple.com/en-mide/guide/mac-help/mchl211c911f/mac).
3. Export once:

   ```sh
   python3 scripts/imessage/sync.py
   ```

The default output is `$HOME/.local/share/augmentagent/imessage-bundle`.
`--out /absolute/private/path` selects another directory. Output inside this
checkout or any Git repository is rejected. The database is opened read-only;
messages and cursor state are written with private permissions. Contact names
are resolved when readable; `--no-contacts` skips that lookup.

## Connect the agent

For a local agent, set this in its `.env`, using your actual absolute path:

```dotenv
AUGMENTAGENT_IMESSAGE_REPO_DIR=/absolute/path/to/private/imessage-bundle
```

The variable keeps its historical name, but the directory does not need Git.
Restart the daemon after setting it. It imports on startup and every 30 minutes.
LLM knowledge capture of new messages is off by default; enable it with
`AUGMENTAGENT_HISTORY_WIKI_CAPTURE=1`.
For person-page backfill, preview and then apply:

```sh
./target/release/augmentagent --wiki-dir /absolute/path/to/wiki imessage sync
./target/release/augmentagent --wiki-dir /absolute/path/to/wiki imessage sync --apply
```

### Linux agent / separate host

Install `rsync` on both hosts and configure SSH key authentication from the Mac
to the agent host. Connect interactively once to verify its host key. On the
agent host, create a dedicated directory owned by the agent user:

```sh
mkdir -p "$HOME/.local/share/augmentagent/imessage-bundle"
chmod 700 "$HOME/.local/share/augmentagent/imessage-bundle"
```

On the Mac, export and mirror to that directory (replace the example destination):

```sh
python3 scripts/imessage/sync.py --remote agent@agent-host:/home/agent/.local/share/augmentagent/imessage-bundle
```

Set `AUGMENTAGENT_IMESSAGE_REPO_DIR` on the receiving agent to that host's absolute
directory, then restart its daemon. The remote directory must already exist and
must be outside the agent's source checkout. Remote paths support letters,
numbers, underscores, dots, slashes and hyphens. Transfers use SSH in batch mode;
the scheduled job must be able to use the key without a password prompt.
Only conversation files and the bundle index are mirrored, without deleting
remote files. The local export cursor and attachment source paths stay on the Mac.
Failed transfers retry on the next run even if there are no new messages.

## Schedule every 30 minutes

After the manual export succeeds, install the Mac job:

```sh
python3 scripts/imessage/schedule.py
# For a separate agent host, pass the same exporter options after --:
python3 scripts/imessage/schedule.py -- --remote agent@agent-host:/home/agent/.local/share/augmentagent/imessage-bundle
```

Choose the command matching your deployment. Include `--out` or other exporter
options again if used for the manual export. Rerunning replaces only this job.
It uses macOS launchd with `RunAtLoad` and `StartInterval`, running immediately,
at login and at 30-minute intervals while awake; see Apple's
[launchd plist reference](https://github.com/apple-oss-distributions/launchd/blob/main/man/launchd.plist.5).
The Mac must be logged in and awake to export; the agent's separate polling
interval means a message may take another 30 minutes to appear after export.

Use `--interval 15` before `--` to change the export interval. The installer prints
the Python path requiring Full Disk Access and the log location. Inspect failures:

```sh
launchctl print "gui/$(id -u)/org.augmentagent.imessage-sync"
tail -n 30 "$HOME/Library/Logs/augmentagent/imessage-sync.log"
```

After granting disk access, trigger a retry with
`launchctl kickstart "gui/$(id -u)/org.augmentagent.imessage-sync"`.
If you move the checkout or Python installation, rerun the installer.
Uninstall with `python3 scripts/imessage/schedule.py --uninstall`; data is retained.

## Attachments and limitations

Attachment metadata is exported by default. Uploading attachment files requires
the AWS CLI and explicit `--s3-bucket YOUR_PRIVATE_BUCKET`, optionally with
`--aws-profile YOUR_PROFILE`. Use your own private bucket and credentials outside
the repo. Attachments already exported without S3 are not retroactively uploaded
by this command; `backfill.py` covers earlier history.

### Setting up the bucket

Attachment upload is opt-in and needs a private bucket plus credentials that can
write to it. Nothing about this is read from the repo.

1. Create a bucket in your own AWS account. Keep it private — block all public
   access, and prefer default encryption. The exporter never changes bucket
   settings and never makes an object public.
2. Create an IAM user or role whose policy allows `s3:PutObject` on
   `arn:aws:s3:::YOUR_BUCKET/*`. Nothing more is required to export; add
   `s3:ListBucket` on `arn:aws:s3:::YOUR_BUCKET` only if you want to audit what
   landed there.
3. Store its credentials in a named AWS profile outside this checkout:

   ```sh
   aws configure --profile YOUR_PROFILE
   ```

4. Verify the credentials resolve before scheduling anything:

   ```sh
   aws sts get-caller-identity --profile YOUR_PROFILE
   ```

5. Export with upload enabled:

   ```sh
   python3 scripts/imessage/sync.py --s3-bucket YOUR_BUCKET --aws-profile YOUR_PROFILE
   ```

Pass the same two options after `--` when installing the schedule, or attachments
export as metadata only. A scheduled job runs without your shell profile, so the
credentials must live in the AWS profile itself and not in environment variables
set by your shell.

### How the upload queue behaves

Every attachment is queued in `.sync_state.json` under `pending_uploads` and
uploaded by a bounded parallel drain — `UPLOAD_WORKERS` files at a time, for at
most `UPLOAD_BUDGET_S` seconds per run. The reference in `messages.md` is written
either way, because the S3 key is deterministic and does not depend on the upload
succeeding.

An item leaves the queue only when it uploaded or its local file is gone.
Anything the run did not attempt stays queued for the next one, so a run that
runs out of time costs progress, never data. Upload failures print the key and
the AWS error to stderr, which the scheduled job writes to its log — check there
first if the queue stops shrinking.

Link previews (`.pluginPayloadAttachment`) are skipped: they are rich-URL card
payloads, not media. They are listed in `SKIP_ATTACHMENT_SUFFIXES`.

Size the interval so a run can finish. A serial upload of a large backlog cannot
complete inside a short interval — each `aws s3 cp` pays a process start, so
hundreds of queued files take far longer than a two-minute tick. If you are
importing years of history, run the first export by hand to completion before
scheduling it, and check the queue afterwards:

```sh
python3 -c "import json;print(len(json.load(open('.sync_state.json'))['pending_uploads']))"
```

The exporter was ported from the existing bundle job and retains its incremental
ROWID cursor and conversation format. It skips tapbacks; attributed-body decoding
is best effort. It imports locally available history and does not propagate edits
or deletions. Keep the output and `.sync_state.json` together; deleting just the
cursor can duplicate history. Do not point two exporters at the same bundle.

## Existing installations and open-source boundaries

Existing cron jobs, private bundle repositories, configuration and data do not
change by adding this code. The daemon still pulls existing Git bundles. New
plain directories bypass Git. The new installer manages only
`org.augmentagent.imessage-sync`; it does not alter any existing crontab.

To migrate later, stop the old exporter first, create a separate private output
directory, run this exporter and point the agent at the new directory. Keep the
old bundle until verification is complete. Do not run both jobs against one
output or copy a private bundle into this source repository.

Only reusable code and synthetic tests belong in the public repo. Messages,
contacts, state, logs, bucket settings and SSH/AWS credentials are private runtime
data. The new exporter never stages, commits or pushes Git files. Ingested history
may be sent to the agent's configured model provider for knowledge capture.

## Sending replies

Replies work like other channels: a new message in an opted-in conversation
becomes an approval card with a draft, and nothing is sent until you approve
it. Approve does not touch Messages. It queues the reply in the agent's
outbox; a sender job on the Mac claims it, sends it with AppleScript and reads
`chat.db` to confirm what happened, then reports back. The action turns
`sent`, or `error` with the reason, and failures are posted as flag notices.

```
agent (macOS or Linux)                    Mac signed in to Messages
new message -> draft -> card
approve -> outbox row
                            <- claim ---  send.py (launchd)
                                          osascript, then verify in chat.db
                            <- complete -
action sent / error
```

The Mac always calls the agent: directly when both run on the same Mac, or
over SSH to a Linux agent. The agent host holds no credentials for the Mac.

### Enable it on the agent

```dotenv
AUGMENTAGENT_IMESSAGE_REPO_DIR=/absolute/path/to/private/imessage-bundle
AUGMENTAGENT_IMESSAGE_SEND_ENABLED=1
# Optional: poll the bundle more often than every 30 minutes.
AUGMENTAGENT_IMESSAGE_POLL_SECS=120
```

`AUGMENTAGENT_IMESSAGE_SEND_ENABLED` is the kill-switch. While it is unset or
`0`, Approve refuses and the outbox hands nothing to the Mac. Restart the
daemon after changing it.

Conversations are opt-in in both directions. Use the conversation identifier
from the bundle index: a phone number or Apple ID email for a 1:1 chat.

```sh
augmentagent imessage allow-inbound  +15555550100  # draft cards for new messages
augmentagent imessage allow-outbound +15555550100  # allow approved replies to send
augmentagent imessage allowlist                    # show both lists
augmentagent imessage deny-outbound  +15555550100
```

Cards are approved from Discord as usual. From a terminal on the agent host:
`augmentagent imessage approve <action_id>` or `augmentagent imessage skip
<action_id>`, with the same checks. `augmentagent imessage outbox list` shows
queued and finished sends without bodies or recipients.

### Install the sender on the Mac

The sender needs two macOS permissions, and which program they attach to
depends on how the job is started:

| Started by | Full Disk Access (read `chat.db`) | Automation (control Messages) |
|---|---|---|
| launchd (`schedule.py --job send`) | the Python the installer prints | `osascript`, prompted on first send |
| cron | `cron` | `cron` → Messages, in System Settings › Privacy & Security › Automation |

A missing Full Disk Access grant makes the sender stop before claiming
anything, with a message naming Full Disk Access. A missing Automation grant
shows up as `osascript timed out` on the reply's error, because macOS cannot
show the permission prompt to a background job; the reply is marked failed,
not retried. Run the first send by hand from a terminal to accept prompts,
and check both grants before relying on the schedule.

For an agent on the same Mac:

```sh
python3 scripts/imessage/send.py --agent-dir /absolute/path/to/agent/checkout
python3 scripts/imessage/schedule.py --job send -- --agent-dir /absolute/path/to/agent/checkout
```

For a Linux agent, use the same SSH setup as the exporter (key-based, batch
mode, host key already accepted) and pass the agent's checkout path on that
host:

```sh
python3 scripts/imessage/send.py --remote agent@agent-host --agent-dir /home/agent/Jarvis
python3 scripts/imessage/schedule.py --job send -- --remote agent@agent-host --agent-dir /home/agent/Jarvis
```

The send job runs every 15 seconds (`--every-seconds` changes it) and reuses
one SSH connection for two minutes. Logs go to
`~/Library/Logs/augmentagent/imessage-send.log`. Uninstall with
`python3 scripts/imessage/schedule.py --job send --uninstall`.

End-to-end latency is the exporter interval plus the agent poll interval for
the card, then the send interval once you approve.

### Delivery guarantees

- `osascript` exits 0 even when delivery fails, so the sender only trusts
  `chat.db`: a sent reply has an outgoing row in the target chat with
  `is_sent = 1` and `error = 0`. Undeliverable sends show `error = 22`.
- At most once. The sender journals each item before sending. If it dies
  mid-send, the next run decides the outcome from `chat.db` and never sends
  that item again. A claim nobody reports within 10 minutes
  (`AUGMENTAGENT_IMESSAGE_CLAIM_TIMEOUT_SECS`) is marked unknown, and you
  are told to check Messages before resending.
- A reply the Mac has not picked up within an hour
  (`AUGMENTAGENT_IMESSAGE_OUTBOX_MAX_AGE_SECS`) expires instead of going
  out late.
- The agent's own sends come back through the exporter and are recognised,
  so they do not create cards. A reply you type yourself retires the
  pending card for that conversation.

### Limits

- 1:1 iMessage conversations, text only. SMS and RCS conversations are
  refused. Group chats need a `chat_guid` in the bundle index (written by
  this exporter, not by older jobs) and their own `allow-outbound` entry;
  cards are never drafted for groups.
- The Mac must be logged in and awake, with Messages signed in.
- No attachments, tapbacks, edits or unsend. BlueBubbles and Messages'
  private API are not used; System Integrity Protection stays enabled.

## Tests

```sh
python3 -m unittest discover -s scripts/imessage/tests -v
cargo test -p augmentagent-channel-imessage
cargo test -p augmentagent-store --test imessage_outbox
cargo test -p augmentagent-cli --test imessage_outbox_cli
cargo test -p augmentagent-cli --bin augmentagent imessage_send
```

Python tests use synthetic SQLite databases and require neither a Mac nor access
to personal messages. Actual Full Disk Access and launchd behavior must be checked
on the exporting Mac.
