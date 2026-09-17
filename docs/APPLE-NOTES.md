# Apple Notes sync

The exporter and scheduler ship in `scripts/apple-notes/`. Your notes are
written to a **private git repository outside this checkout**; the agent reads
that checkout. Notes are mutable documents, so unlike the iMessage bundle the
output is *meant* to be a git repository: every edit is a diff, every rename
follows in history, every deletion is a commit. This integration reads notes;
it never writes to Notes.app.

## What gets exported

- Every note that is not password-protected and not in Recently Deleted, as
  `notes/<folder>/<title>.md` with YAML frontmatter (`identifier`, `title`,
  `folder`, `account`, `created`, `modified`, `attachments`, `redactions`)
  and the note's plain text. Rich formatting (tables, checklists, styles) is
  flattened; inline attachments become `[attachment: <mime> <filename>]` lines.
- `notes/index.json` keyed by the note's stable UUID (title, folder, path,
  dates, redactions), `notes/index.md` for humans, and an OKF `index.md` root.
- `.sync_state.json` holding a per-note content hash so a re-save that changes
  nothing produces no write and no commit.

A deleted note loses its file and keeps a `deleted: <time>` tombstone in
`index.json` for 30 days so consumers can retire their copy.

## Secret scrubbing

Every body passes through `scripts/apple-notes/scrub.py` before it is
written, and `sync.py --commit` re-scans the whole bundle and refuses to
commit on any finding. Redacted spans read `[REDACTED:<kind>]` and the
frontmatter lists the kinds under `redactions:`.

Caught: PEM private keys (multi-line, inline, or with literal `\n`), AWS
access and secret keys, GitHub/Slack/OpenAI/Anthropic/Stripe/Google tokens,
JWTs, `password|passwd|pwd|secret|token|api_key = value` assignments,
`pin|passcode = digits`, and Luhn-valid card numbers (grouped, or bare on a
line that mentions a card).

**Not caught, by design:** a bare high-entropy string with no keyword or known
prefix, a password stated in prose ("my wifi password is hunter2"), account
numbers, and social security numbers. Handle those with the quarantine list.

A note whose title (its first line) contains a secret is quarantined
entirely, never written, because the title becomes the filename.

### Quarantine list

Create `<out>/config.json`:

```json
{
  "skip_folders": ["Passwords"],
  "skip_notes": ["7BED7E65-9973-4633-908C-CE62B5B03BF3"]
}
```

Quarantined notes are recorded under `skipped` in `.sync_state.json` with a
reason (`skip-folder`, `skip-note`, `title-secret`). Adding a previously
exported note to the list removes its file on the next run. Find a note's UUID
in `notes/index.json`.

## On the Mac

Python 3.9 or newer, standard library only. The Notes database
(`~/Library/Group Containers/group.com.apple.notes/NoteStore.sqlite`) has been
readable without Full Disk Access on recent macOS; if a run reports it cannot
read the database, grant access to the Python executable the scheduler prints.

1. Create the private repository and point the exporter at it:

   ```sh
   mkdir -p ~/AppleNotesSync && git -C ~/AppleNotesSync init -b main
   printf '.sync.lock\n__pycache__/\n.DS_Store\n' > ~/AppleNotesSync/.gitignore
   python3 scripts/apple-notes/sync.py --out ~/AppleNotesSync
   ```

2. Read the summary, then check the bundle before the first commit:

   ```sh
   python3 scripts/apple-notes/scrub.py --check ~/AppleNotesSync   # exits 1 on any finding
   grep -rl 'REDACTED' ~/AppleNotesSync/notes | wc -l               # how many notes were scrubbed
   ```

   Add anything you would rather not export at all to `config.json` and run
   again. Output inside this source checkout is rejected.

3. Commit and push (add a private remote first if you want one):

   ```sh
   gh repo create AppleNotesSync --private --source ~/AppleNotesSync --remote origin
   python3 scripts/apple-notes/sync.py --out ~/AppleNotesSync --commit
   ```

   Commits are titled `Sync notes <date>: N new, N updated, N renamed, N deleted`
   and list the touched titles. Without a remote the commit is kept locally.

## Schedule every 2 minutes

```sh
python3 scripts/apple-notes/schedule.py -- --out ~/AppleNotesSync --commit
```

Installs `org.augmentagent.apple-notes-sync` in launchd with `RunAtLoad` and a
2-minute `StartInterval` (change with `--interval N` before `--`). A run with
no changes takes well under a second and commits nothing. An exclusive lock
makes an overlapping run exit immediately. Inspect:

```sh
launchctl print "gui/$(id -u)/org.augmentagent.apple-notes-sync"
tail -n 30 "$HOME/Library/Logs/augmentagent/apple-notes-sync.log"
```

Uninstall with `python3 scripts/apple-notes/schedule.py --uninstall`; the
exported notes are retained.

## Connect the agent

Set in the agent's `.env`:

```dotenv
AUGMENTAGENT_APPLE_NOTES_REPO_DIR=/absolute/path/to/AppleNotesSync
```

The daemon pulls the checkout and imports new and edited notes into
searchable history; see the tracking issue (#1062) for the ingest pieces.

## Tests

```sh
python3 -m unittest discover -s scripts/apple-notes/tests -v
```

Tests build a synthetic `NoteStore.sqlite` with the subset of Apple's schema
the sync reads and encode note bodies with a tiny protobuf writer, so they
need neither a Mac nor anyone's real notes. Only reusable code and synthetic
fixtures belong in this repository; notes, state, config and logs are private
runtime data.
