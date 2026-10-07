# Disk hygiene

Cargo never deletes an artifact, so every target dir this project builds into
grows until the filesystem is full. Epic #1406 gives each unbounded writer an
owner and a cap: build output, temp files, deploy snapshots, the database and
the logs.

## What runs

| What | When | Does |
|---|---|---|
| `augmentagent disk prune` | daily (`augmentagent-disk-prune.timer`) | Removes cold build output from every target dir the repo owns |
| `augmentagent log-rotate` | daily, same timer | Rotates and compresses the state dir's logs and sinks |
| `augmentagent ops-archive sync` | daily, same timer | Pushes rotated logs to a private repo, if one is configured |
| `augmentagent deploy prune` | daily, and after every verified update | Keeps the newest deploy snapshots and rollback binaries |
| action-body compaction | hourly, inside the daemon | Drops action bodies that only repeat the stored email |
| gate cache trim | every updater tick, lanes idle | Brings the gate cache back under `AUGMENTAGENT_GATE_CACHE_MAX_MB` in tiers |
| gate free-space floor | before every gate | Refuses to start below `AUGMENTAGENT_GATE_MIN_FREE_GB` |
| `autopr-health` | every 2 h | Alerts on any watched filesystem under the floor, or shrinking fast |

`augmentagent disk status` prints free space per filesystem and what lives
there. `augmentagent disk prune --dry-run` prints what a pass would remove.

## What counts as cold

Pruning never asks "how old is this file". It asks "has any compile needed
this unit lately", answered from cargo's own fingerprint records: a unit is in
use as of the newest compile of anything that depends on it. A third-party
rlib compiled a month ago and linked by today's build is warm; an rlib from a
feature set or a worktree nobody has built for a week is cold.

That is why a prune does not slow the next build: nothing a recent compile
depended on is touched. Access times are deliberately not used, because build
volumes are commonly mounted `noatime`.

Three rules follow from it:

- `incremental/` sessions go after 2 days idle.
- Artifacts go after 7 days without a compile needing them.
- A worktree whose branch is merged (or whose upstream is gone) loses its whole
  `target/` once nothing has been written there for a day.

And two refusals:

- A target dir a running `cargo`/`rustc` is using is skipped whole. So is the
  gate cache while an auto-PR lane holds its lock.
- If the fingerprint records stop resolving (a cargo format change), artifacts
  are kept and only `incremental/` is pruned. The report says so.

Top-level binaries in a profile dir, which is where the deployed daemon
lives, are never candidates.

## Temp-dir leftovers

A test process that is killed (a timeout, Ctrl-C, an OOM) never runs its
destructors, so its `tempfile` entries stay in the system temp dir: SQLite
files with `-wal`/`-shm` sidecars and whole `.tmpXXXXXX` dirs. A clean test run
leaves nothing, so this cannot be fixed in the tests. `disk prune` sweeps:

- sidecars whose database is already gone, after a day;
- a database with its sidecars, after a day, when no running process has it
  open;
- a `.tmpXXXXXX` dir, after a week, when no running process has a file open in
  it or sits inside it.

Only names the `tempfile` crate generates, owned by the current user, are ever
considered. Where open files cannot be listed (no `/proc`), only the first
rule applies.

The gate keeps its own temp files inside the gate cache (#877) and sweeps them
before every run. The renderer sidecar removes its webpack bundle dir on
shutdown and, at start, the bundle dirs of renderers that are no longer
running.

## Which dirs

- the checkout's `target/` (profile dirs reached through a symlink are pruned
  through the link; the link target's parent is never treated as ours);
- every registered git worktree's `target/`;
- the gate cache (`AUGMENTAGENT_GATE_TARGET_DIR`);
- anything in `AUGMENTAGENT_PRUNE_TARGET_DIRS` (colon-separated). Use it for
  `CARGO_TARGET_DIR` overrides the repo cannot discover.

## The gate cache

The gate builds with `CARGO_PROFILE_DEV_DEBUG=0` and
`CARGO_PROFILE_TEST_DEBUG=0`. Debuginfo was most of what a gate run wrote and
the gate never reads it.

When the cache is over its cap and every lane is idle, the updater trims in
tiers and stops as soon as it fits:

1. all of `incremental/`;
2. artifacts no compile has needed for `AUGMENTAGENT_GATE_CACHE_STALE_DAYS`,
   longest-idle first;
3. the whole `debug/` (the pre-#1407 behaviour, one cold rebuild). Also the
   fallback when the deployed binary predates `disk prune`.

A gate that would start with less than `AUGMENTAGENT_GATE_MIN_FREE_GB` free is
refused as an infra failure: it is not charged to the builder, and a refusal
during the baseline check is never recorded as a red `main`.

## Deploy snapshots

`augmentagent deploy snapshot --label <name>` is the supported way to back up
before a manual deploy. It writes one dated dir under
`~/.local/share/augmentagent/deploy-snapshots/` (override:
`AUGMENTAGENT_DEPLOY_SNAPSHOT_DIR`) holding:

- `data.db.gz` — taken with `VACUUM INTO`, so it is consistent while the
  daemon runs, then compressed (about 3.5x smaller on a mail database);
- `augmentagent` — the release binary;
- `manifest.json` — time, label, commit, sizes.

`deploy list` shows them. `deploy restore-db <name> --to <path>` writes the
database back; stop the daemon first, and pass `--force` to replace a file.

`deploy prune` keeps the newest `AUGMENTAGENT_DEPLOY_KEEP` (default 2) and
anything younger than 48 hours, and applies the same rule to the
`augmentagent.*` rollback binaries beside the release build. The newest is
never removed. It runs after every verified update and with the daily timer.

Backups made by hand before this existed (`deploy-backups/`,
`deploy-artifacts/`, the state dir's `rollbacks/`) are left alone unless asked:
`deploy prune --strays` lists the ones older than 7 days, and `--strays --yes`
removes them, always keeping the newest in each place.

## The database

`actions.originalBody` repeated `emails.body` for every action that never
produces a reply, which is most of them: about 40% of the file on a mail-heavy
instance. The daemon drops that duplicate hourly, in small batches:

- only for actions in a terminal no-reply status (`skipped`,
  `permanent_error`) older than `AUGMENTAGENT_ACTION_BODY_RETENTION_DAYS`;
- only when the email row holds byte-for-byte the same text.

No row and no decision is removed. Readers (the store's
`get_action_with_email`, the dashboard) fall back to the email's body, so they
show what they always showed.

Dropping text frees pages inside the file; the file shrinks only on a
`VACUUM`, which rewrites the whole database and is therefore never automatic:

```sh
augmentagent db compact --dry-run     # what would be dropped
augmentagent db compact --vacuum      # drop it and shrink the file; run when quiet
```

`doctor`'s `db_size` check reports the file size, the largest tables and how
much is reclaimable. Deploy snapshots are compact regardless: `VACUUM INTO`
never copies free pages.

## Logs

Every unit appends to a log in the state dir and the in-process sinks
(`tool-audit.log`, `token-usage.jsonl`) append forever. `augmentagent
log-rotate` runs with the daily timer:

- a file is rotated once it reaches `AUGMENTAGENT_LOG_ROTATE_MB`, or once a
  calendar month has passed since it was last rotated (or created);
- rotated files are gzip-compressed as `<name>.<YYYYMMDD>.gz` and kept for
  `AUGMENTAGENT_LOG_KEEP_MONTHS`, then deleted.

A file nobody holds open is renamed, which is atomic and loses nothing; the
next writer creates a fresh file. That covers the timer units and the sinks.
A file a process holds open (the daemon's own stdout and stderr) is copied
and then truncated in place, and a line written in the instant between the
two can be lost. A held `.jsonl` is skipped rather than risk half a record.

Readers that look back across a rotation still work: `token-usage` reads the
kept rotations too, and `autopr-health` takes the tail of the newest rotation
when the live log is shorter than its window.

## Archive or prune

The rule: **archive only what is small, text and cannot be regenerated; prune
everything else.**

| Data | Decision | Why |
|---|---|---|
| Cargo target dirs, gate cache, worktree builds | Prune | Rebuildable from source |
| Temp files, render bundles | Prune | Leftovers of dead processes |
| Old rollback binaries and deploy snapshots | Prune, keep the last N | Rebuildable from the commit |
| Reasoner handoff journals | Prune (the daemon's hourly sweep) | Short-lived by design |
| CLI session transcripts | Prune (the CLI's own 30-day cleanup) | Large, low reuse |
| Rotated daemon logs | Archive | The only record of past incidents; about 20x smaller compressed |
| `tool-audit.log`, `token-usage.jsonl` | Archive | Audit trail and cost history |
| The live database | Neither | Too big for git: needs a real backup target |

The archive is opt-in. Without it, rotation simply deletes after local
retention.

```sh
scripts/ops-archive-bootstrap.sh            # creates a PRIVATE repo, once
# then add to .env:  AUGMENTAGENT_OPS_ARCHIVE_REMOTE=<owner>/<repo>
augmentagent ops-archive sync --dry-run
```

`ops-archive sync` runs with the daily timer and:

- takes only closed, compressed rotations from the state dir. A live file, the
  database or `.env` is never a candidate, and an allowlist over the staged
  paths aborts the commit if anything else is there;
- refuses to push unless GitHub reports the remote as `PRIVATE`. The logs hold
  message content;
- skips and reports any file containing one of the daemon's own secret values
  or a known credential prefix;
- after a successful push, deletes local rotations past
  `AUGMENTAGENT_LOG_KEEP_MONTHS`.

## Settings

| Variable | Default | Meaning |
|---|---|---|
| `AUGMENTAGENT_PRUNE_ARTIFACT_DAYS` | 7 | Days without a compile needing a unit before it is cold |
| `AUGMENTAGENT_PRUNE_INCREMENTAL_DAYS` | 2 | Idle days before an `incremental/` session goes |
| `AUGMENTAGENT_PRUNE_TARGET_DIRS` | unset | Extra target dirs, colon-separated |
| `AUGMENTAGENT_GATE_MIN_FREE_GB` | 15 | Free-space floor for a gate; `0` disables |
| `AUGMENTAGENT_GATE_CACHE_MAX_MB` | 20000 | Gate cache cap |
| `AUGMENTAGENT_GATE_CACHE_STALE_DAYS` | 3 | Idle days before a cap trim may take an artifact |
| `AUGMENTAGENT_ACTION_BODY_RETENTION_DAYS` | 30 | Days a terminal action keeps its own copy of the body; `0` disables compaction |
| `AUGMENTAGENT_LOG_ROTATE_MB` | 50 | Size at which a log is rotated regardless of age |
| `AUGMENTAGENT_LOG_KEEP_MONTHS` | 3 | Months a rotated log is kept locally |
| `AUGMENTAGENT_OPS_ARCHIVE_REMOTE` | unset | `owner/repo` of the private ops archive; unset = off |
| `AUGMENTAGENT_OPS_ARCHIVE_DIR` | data dir | Local clone of the ops archive |
| `AUGMENTAGENT_DEPLOY_KEEP` | 2 | Deploy snapshots and rollback binaries kept regardless of age |
| `AUGMENTAGENT_DEPLOY_SNAPSHOT_DIR` | data dir | Where deploy snapshots are written |

## Install the timer

The updater does not install unit files.

```sh
cp scripts/systemd/augmentagent-disk-prune.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now augmentagent-disk-prune.timer
```

Log: `~/.local/state/augmentagent/disk-prune.log`.
