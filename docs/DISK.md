# Disk hygiene

Cargo never deletes an artifact, so every target dir this project builds into
grows until the filesystem is full. Epic #1406 gives each unbounded writer an
owner and a cap. This page covers the build-output half.

## What runs

| What | When | Does |
|---|---|---|
| `augmentagent disk prune` | daily (`augmentagent-disk-prune.timer`) | Removes cold build output from every target dir the repo owns |
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

## Settings

| Variable | Default | Meaning |
|---|---|---|
| `AUGMENTAGENT_PRUNE_ARTIFACT_DAYS` | 7 | Days without a compile needing a unit before it is cold |
| `AUGMENTAGENT_PRUNE_INCREMENTAL_DAYS` | 2 | Idle days before an `incremental/` session goes |
| `AUGMENTAGENT_PRUNE_TARGET_DIRS` | unset | Extra target dirs, colon-separated |
| `AUGMENTAGENT_GATE_MIN_FREE_GB` | 15 | Free-space floor for a gate; `0` disables |
| `AUGMENTAGENT_GATE_CACHE_MAX_MB` | 20000 | Gate cache cap |
| `AUGMENTAGENT_GATE_CACHE_STALE_DAYS` | 3 | Idle days before a cap trim may take an artifact |

## Install the timer

The updater does not install unit files.

```sh
cp scripts/systemd/augmentagent-disk-prune.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now augmentagent-disk-prune.timer
```

Log: `~/.local/state/augmentagent/disk-prune.log`.
