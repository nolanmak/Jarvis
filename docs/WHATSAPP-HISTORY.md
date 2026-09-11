# WhatsApp history → Jarvis

Jarvis includes a WhatsApp Desktop exporter, a Mac scheduler and a read-only
archive importer. Messages are searchable through the agent's memory tools;
subsequent new messages can update the wiki. This feature does not enable the
separate live WhatsApp channel, linked-device pairing, triage or outbound sends.

## Export on the Mac

Use Python 3.9+ and WhatsApp Desktop signed in with its history downloaded. Run
from this source checkout; no agent build is needed on the exporting Mac:

```sh
python3 scripts/whatsapp/sync.py
```

The exporter reads
`~/Library/Group Containers/group.net.whatsapp.WhatsApp.shared/ChatStorage.sqlite`
using SQLite's backup API with a read-only source connection, including committed
WAL content. If the app uses another location, supply `--db /absolute/path`.
Schema changes in WhatsApp Desktop may require exporter updates.

The output defaults to `~/.local/share/augmentagent/whatsapp-bundle`.
`--out /absolute/private/path` selects a different directory. It must be outside
all Git checkouts. The exporter never commits or pushes files. Output permissions
are private, concurrent runs use a lock, and indexes/cursor files are replaced
atomically. Keep the bundle and `.sync_state.json` together. As with the original
exporter, interruption between message append and cursor save can require manual
duplicate cleanup; deleting just the cursor also duplicates exports.

If macOS denies database access, grant your terminal and the scheduled Python
executable Full Disk Access in System Settings → Privacy & Security. Find the
Python executable with `python3 -c 'import sys; print(sys.executable)'`. See Apple's
[Full Disk Access settings](https://support.apple.com/en-mide/guide/mac-help/mchl211c911f/mac).

## Connect a local or Linux agent

For a local agent, set its `.env` to the actual absolute export directory:

```dotenv
AUGMENTAGENT_WHATSAPP_HISTORY_DIR=/absolute/private/whatsapp-bundle
```

For a separate Linux agent, install `rsync` on both hosts. Create a dedicated
private directory on the receiver, outside the source checkout:

```sh
mkdir -p "$HOME/.local/share/augmentagent/whatsapp-bundle"
chmod 700 "$HOME/.local/share/augmentagent/whatsapp-bundle"
```

Configure SSH key authentication from the Mac and verify the host key by logging
in once interactively. Then export and mirror from the Mac:

```sh
python3 scripts/whatsapp/sync.py --remote agent@agent-host:/home/agent/.local/share/augmentagent/whatsapp-bundle
```

Replace the destination with your own host and absolute directory; remote paths
support letters, numbers, dots, underscores, hyphens and slashes. Scheduled SSH
uses batch mode and needs credentials available without a password prompt.
Only conversation files and the bundle index are mirrored; local cursor state
stays on the Mac. Failed transfers retry on subsequent runs even without new
messages. Remote files are not deleted. The bundle index selects active files.

Set `AUGMENTAGENT_WHATSAPP_HISTORY_DIR` on the receiver to its local directory.
Build the agent and memory server, perform the initial import, and restart your
usual daemon service:

```sh
cargo build --release -p augmentagent-cli -p augmentagent-mcp-memory
./target/release/augmentagent whatsapp-history poll-once
# On installations using the included systemd service:
systemctl --user restart augmentagent.service
```

`poll-once` pulls a Git feed when present, imports history and prints counts; it
does not call a model. The daemon polls immediately and every 30 minutes.
With a configured wiki, it submits new entries in previously imported
conversations to the existing bounded knowledge-capture queue. The first import
of each conversation populates searchable history without a mass model backfill.
Wiki capture is best effort; full imported messages remain searchable if capture
fails or its queue is full. Allow up to another polling interval after an export.

## Schedule exports

After a manual export succeeds, install on the Mac:

```sh
python3 scripts/whatsapp/schedule.py
# Or, for a separate receiver:
python3 scripts/whatsapp/schedule.py -- --remote agent@agent-host:/home/agent/.local/share/augmentagent/whatsapp-bundle
```

Choose the appropriate command and repeat any `--out` / `--db` overrides after
`--`. Use `--interval 15` before `--` for a different interval. This installs the
`org.augmentagent.whatsapp-history-sync` launchd agent, immediately and every 30
minutes while logged in and awake. Existing crontabs and iMessage jobs are untouched.

```sh
launchctl print "gui/$(id -u)/org.augmentagent.whatsapp-history-sync"
tail -n 30 "$HOME/Library/Logs/augmentagent/whatsapp-history-sync.log"
python3 scripts/whatsapp/schedule.py --uninstall
```

Uninstall removes only this schedule and retains the data. Rerun the installer
after moving the checkout or Python. macOS access and scheduling need verification
on the Mac; synthetic Linux tests cannot verify those permissions.

## Existing private Git feeds

Keep the existing exporter/cron job. Clone its private data repo to a private
directory on the agent host and set `AUGMENTAGENT_WHATSAPP_HISTORY_DIR` to it.
The daemon performs a fast-forward-only pull with a timeout and no credential
prompts; failure leaves it reading the last on-disk bundle. Plain directories
skip Git. No private feed URL is hardcoded in Jarvis.

The bundled exporter rejects Git output to prevent accidental publication. To
migrate the exporting side, stop its old job first and use a separate private
output directory, then switch the receiver configuration. Preserve old data
until verification; do not run two exporters into one output directory.

## Ask Jarvis about a conversation

Ask for a topic or contact, for example “What did we decide about the cabin on
WhatsApp?” The agent can call `search_conversation_history` with
`channel: "whatsapp"`, then `read_conversation_thread` with a returned `thread_id`.
Thread reads include speakers, dates and message text, and paginate large bodies
without losing the text after a snippet. Pass both continuation offsets from
each page to the next call. These tools read already imported data; they do not
contact WhatsApp or send messages.

The exporter includes text and attachment labels, not media downloads, deleted
messages or edit propagation. Broadcast/status sessions and empty system events
are skipped. Only history present in the Mac's database can be imported.
Malformed timestamps are skipped; unreadable conversations are counted and
retried later. Append-only message order is the stable ID contract: don't reorder
or replace an imported bundle with unrelated history under the same identifiers.

Conversation content, contacts, state and credentials stay out of the public
repo. The agent's configured model provider may receive conversation text during
queries or wiki capture. Logs from your own deployment can contain private paths;
share counts rather than private message content in public issues.

## Tests

```sh
python3 -m unittest discover -s scripts/whatsapp/tests -v
cargo test -p augmentagent-channel-whatsapp-history
cargo test -p augmentagent-mcp-memory
cargo test -p augmentagent-cli --test whatsapp_history_cli
```

All fixtures are synthetic. Integration tests exercise the actual Python export,
Rust import, historical search, repeat polling, and paginated memory-tool reads.
