# iMessage history sync

The exporter and scheduler ship in `scripts/imessage/`. You need one source
repository; message data lives in a private directory outside it. A Mac with
Messages synced to your account exports history. The agent can read that directory
locally or receive it over SSH on Linux. This integration imports history; it
does not send iMessages.

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
With a configured wiki, subsequent new messages also trigger knowledge capture.
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
the repo. Failed uploads remain queued for later runs. Attachments already
exported without S3 are not retroactively uploaded by this command.

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

## Tests

```sh
python3 -m unittest discover -s scripts/imessage/tests -v
cargo test -p augmentagent-channel-imessage
```

Python tests use synthetic SQLite databases and require neither a Mac nor access
to personal messages. Actual Full Disk Access and launchd behavior must be checked
on the exporting Mac.
