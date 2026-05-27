# Swarm resource budget

Operational guide for sizing parallel swarm runs and keeping the host from
filling up. Pair with `scripts/swarm-gate.sh` (pre-flight) and
`scripts/worktree-prune.sh` (post-run cleanup).

## Per-worker disk budget

| Component                      | Typical | Worst case |
| ------------------------------ | ------- | ---------- |
| Source checkout in worktree    | ~0.5 GB | ~0.7 GB    |
| `target/` (Rust release build) | ~3 GB   | ~4.5 GB    |
| `node_modules/` (shared)       | n/a     | n/a        |
| **Per-worker total**           | ~3.5 GB | **~5 GB**  |

Plan on **5 GB per worker** as the worst-case figure. Add a **10 GB safety
margin** above that for cargo/npm caches, log rotation, and the OS itself.

## Default concurrency

Three parallel workers is the comfort zone for a host with **>=70 GB free**:

```
3 workers × 5 GB peak  =  15 GB worker target overhead
                       +  10 GB safety margin
                       = ~25 GB minimum free space
```

The default `swarm-gate.sh --check-disk 15` floor is intentionally
conservative for the *minimum* a single dispatcher run needs free; bump it
with `--check-disk 25` (or higher) if you're spawning the full 3-worker fan-out
from cold caches.

`--max-worktrees 4` is the default ceiling on
`.claude/worktrees/*` entries to avoid runaway fan-out when a previous swarm
left workers behind.

## Sharing `CARGO_TARGET_DIR`

- **Sequential workers** can safely point at a single shared target dir
  (e.g. `CARGO_TARGET_DIR=$REPO_ROOT/target`). This cuts per-worker build
  cost dramatically after the first run.
- **Concurrent workers must NOT share `CARGO_TARGET_DIR`.** Cargo takes
  file locks on the target directory and concurrent builds against the same
  target serialize at best, deadlock at worst. Let each worker keep its own
  `target/` inside its worktree, and reclaim space afterward via
  `worktree-prune.sh --trim-targets`.

## Pre-flight (`scripts/swarm-gate.sh`)

```
# Full default: build + test + npm + default resource gate.
bash scripts/swarm-gate.sh

# Resource gate only (no build), 25 GB disk floor, cap at 3 worktrees.
bash scripts/swarm-gate.sh --check-disk 25 --max-worktrees 3 --no-build

# Show usage.
bash scripts/swarm-gate.sh --help
```

`swarm-gate.sh` prints a one-line summary (`Pre-flight: N GB free, M active
worktrees`) before checking. It exits non-zero if either limit is breached,
so a swarm dispatcher can `set -e` on the gate and skip spawning rather than
fail mid-run.

## Cleanup (`scripts/worktree-prune.sh`)

Run after each swarm batch — or at least after each PR merges — to reclaim
the per-worker `target/` overhead and clear stale branch refs.

```
# Dry-run the full clean.
bash scripts/worktree-prune.sh --dry-run --all

# Actually do it.
bash scripts/worktree-prune.sh --all
```

Individual modes:

- `--merged` deletes local `worktree-agent-*` branches whose tip is an
  ancestor of `origin/main`. Skips any branch backing an active worktree.
- `--trim-targets` removes `target/` inside each
  `.claude/worktrees/*/`. Source trees are left intact. Reports MB
  recovered.
- `--orphan-worktrees` runs `git worktree remove --force` for any entry
  whose branch ref is gone.

The script always prints a summary table:

```
==================== worktree-prune summary ====================
  branches deleted   : 3
  MB recovered       : 8421
  worktrees pruned   : 1
================================================================
```

All operations are idempotent.
