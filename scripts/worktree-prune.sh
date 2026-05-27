#!/bin/bash
# worktree-prune.sh — reclaim disk and clean up stale state left behind by
# swarm runs.
#
# Three independent jobs (combinable via --all):
#   --merged             Delete local worktree-agent-* branches whose tip is
#                        an ancestor of origin/main. Skips branches that
#                        currently back an active worktree (we'd never want
#                        to yank a checked-out branch out from under a
#                        running worker).
#   --trim-targets       For each directory under .claude/worktrees/*/,
#                        delete the `target/` build artifact directory.
#                        Source trees are left alone. Reports MB recovered.
#   --orphan-worktrees   For each git worktree whose branch ref no longer
#                        exists, `git worktree remove --force` the path.
#   --all                Shorthand for the three above.
#   --dry-run            Print what would happen, do nothing.
#   -h, --help           Show usage.
#
# Default (no flags) is "print usage and exit 0" so accidental invocations
# are harmless. All operations are idempotent and safe to re-run.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORKTREE_DIR="$REPO_ROOT/.claude/worktrees"

DRY_RUN=0
DO_MERGED=0
DO_TRIM=0
DO_ORPHAN=0

BRANCHES_DELETED=0
MB_RECOVERED=0
WORKTREES_PRUNED=0

usage() {
  cat <<'USAGE'
Usage: worktree-prune.sh [--dry-run] [--merged] [--trim-targets] [--orphan-worktrees] [--all] [-h|--help]

Reclaim disk and clean stale state from swarm worktrees.

Modes (combinable):
  --merged             Delete local worktree-agent-* branches already merged
                       into origin/main. Skips branches backing an active
                       worktree.
  --trim-targets       Delete target/ inside each .claude/worktrees/*/.
                       Source files are not touched.
  --orphan-worktrees   Remove worktrees whose backing branch is gone.
  --all                --merged + --trim-targets + --orphan-worktrees.

Flags:
  --dry-run            Report what would happen; make no changes.
  -h, --help           Show this help.

With no mode flags, prints usage and exits 0.
USAGE
}

log() {
  printf '[worktree-prune] %s\n' "$*"
}

# Resolve the full path to a directory, with realpath if available, else
# python3 (POSIX abspath), else fallback to printf.
abs_path() {
  local p="$1"
  if command -v realpath >/dev/null 2>&1; then
    realpath "$p"
  elif command -v python3 >/dev/null 2>&1; then
    python3 -c 'import os, sys; print(os.path.abspath(sys.argv[1]))' "$p"
  else
    # Best-effort: leave as-is if already absolute, else prefix with PWD.
    case "$p" in
      /*) printf '%s\n' "$p" ;;
      *)  printf '%s/%s\n' "$PWD" "$p" ;;
    esac
  fi
}

# Print the set of branch refs that currently back an active git worktree,
# one per line (e.g. "refs/heads/feat/foo"). Used to avoid deleting a
# branch out from under a running worker.
active_branches() {
  git worktree list --porcelain \
    | awk '/^branch /{print $2}'
}

# Print sizes of a path in MB, rounded down. 0 if missing.
size_mb() {
  local p="$1"
  if [ -d "$p" ]; then
    # du -sm gives megabytes (1024*1024). Awk strips the path field.
    du -sm "$p" 2>/dev/null | awk '{print $1}'
  else
    echo 0
  fi
}

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    --merged) DO_MERGED=1; shift ;;
    --trim-targets) DO_TRIM=1; shift ;;
    --orphan-worktrees) DO_ORPHAN=1; shift ;;
    --all)
      DO_MERGED=1
      DO_TRIM=1
      DO_ORPHAN=1
      shift
      ;;
    -h|--help) usage; exit 0 ;;
    *)
      printf '[worktree-prune ERR] unknown argument: %s\n' "$1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

# Default: print usage and bail without doing anything.
if [ "$DO_MERGED" -eq 0 ] && [ "$DO_TRIM" -eq 0 ] && [ "$DO_ORPHAN" -eq 0 ]; then
  usage
  exit 0
fi

cd "$REPO_ROOT"

if [ "$DRY_RUN" -eq 1 ]; then
  log "DRY-RUN: no changes will be made"
fi

# ---------------------------------------------------------------------------
# --merged: delete local worktree-agent-* branches already in origin/main.
# ---------------------------------------------------------------------------
if [ "$DO_MERGED" -eq 1 ]; then
  log "scanning local worktree-agent-* branches for merged candidates"

  # Build a lookup of branches backing active worktrees.
  active_set=""
  while IFS= read -r br; do
    [ -n "$br" ] || continue
    active_set+="${br}"$'\n'
  done < <(active_branches)

  # Iterate all matching local branches.
  while IFS= read -r br; do
    [ -n "$br" ] || continue
    ref="refs/heads/$br"
    if printf '%s' "$active_set" | grep -Fxq "$ref"; then
      log "  skip $br (backs an active worktree)"
      continue
    fi
    if git merge-base --is-ancestor "$br" origin/main 2>/dev/null; then
      if [ "$DRY_RUN" -eq 1 ]; then
        log "  would delete $br"
      else
        log "  deleting $br"
        if git branch -D "$br" >/dev/null 2>&1; then
          BRANCHES_DELETED=$((BRANCHES_DELETED + 1))
        else
          log "  WARN: failed to delete $br"
        fi
      fi
      # Count dry-run candidates too, so the summary is informative.
      if [ "$DRY_RUN" -eq 1 ]; then
        BRANCHES_DELETED=$((BRANCHES_DELETED + 1))
      fi
    fi
  done < <(git for-each-ref --format='%(refname:short)' 'refs/heads/worktree-agent-*')
fi

# ---------------------------------------------------------------------------
# --trim-targets: delete target/ inside each active worktree dir.
# ---------------------------------------------------------------------------
if [ "$DO_TRIM" -eq 1 ]; then
  if [ -d "$WORKTREE_DIR" ]; then
    log "scanning $WORKTREE_DIR for reclaimable target/ directories"
    while IFS= read -r wt; do
      [ -n "$wt" ] || continue
      target="$wt/target"
      [ -d "$target" ] || continue
      mb="$(size_mb "$target")"
      if [ "$DRY_RUN" -eq 1 ]; then
        log "  would remove $target (${mb} MB)"
      else
        log "  removing $target (${mb} MB)"
        rm -rf "$target"
      fi
      MB_RECOVERED=$((MB_RECOVERED + mb))
    done < <(find "$WORKTREE_DIR" -mindepth 1 -maxdepth 1 -type d)
  else
    log "no $WORKTREE_DIR — nothing to trim"
  fi
fi

# ---------------------------------------------------------------------------
# --orphan-worktrees: remove worktrees whose branch ref is gone.
# ---------------------------------------------------------------------------
if [ "$DO_ORPHAN" -eq 1 ]; then
  log "scanning git worktrees for orphans (branch ref missing)"

  # Parse `git worktree list --porcelain`. Records are blank-line separated.
  # We collect (path, branch) per record. If `branch` is absent (detached),
  # we leave the worktree alone — operator intervention required.
  cur_path=""
  cur_branch=""
  flush_record() {
    if [ -n "$cur_path" ] && [ -n "$cur_branch" ]; then
      local main_path
      main_path="$(abs_path "$REPO_ROOT")"
      # Never prune the primary worktree.
      if [ "$cur_path" = "$main_path" ]; then
        cur_path=""; cur_branch=""
        return
      fi
      local short="${cur_branch#refs/heads/}"
      if ! git show-ref --verify --quiet "$cur_branch"; then
        if [ "$DRY_RUN" -eq 1 ]; then
          log "  would remove worktree $cur_path (branch $short is gone)"
        else
          log "  removing worktree $cur_path (branch $short is gone)"
          git worktree remove --force "$cur_path" >/dev/null 2>&1 \
            || log "  WARN: git worktree remove failed for $cur_path"
        fi
        WORKTREES_PRUNED=$((WORKTREES_PRUNED + 1))
      fi
    fi
    cur_path=""
    cur_branch=""
  }

  while IFS= read -r line; do
    case "$line" in
      "worktree "*) cur_path="${line#worktree }" ;;
      "branch "*)   cur_branch="${line#branch }" ;;
      "")           flush_record ;;
    esac
  done < <(git worktree list --porcelain; printf '\n')
  flush_record
fi

# ---------------------------------------------------------------------------
# Summary table.
# ---------------------------------------------------------------------------
echo
echo "==================== worktree-prune summary ===================="
if [ "$DRY_RUN" -eq 1 ]; then
  printf '  branches that would be deleted : %s\n' "$BRANCHES_DELETED"
  printf '  MB that would be recovered     : %s\n' "$MB_RECOVERED"
  printf '  worktrees that would be pruned : %s\n' "$WORKTREES_PRUNED"
else
  printf '  branches deleted   : %s\n' "$BRANCHES_DELETED"
  printf '  MB recovered       : %s\n' "$MB_RECOVERED"
  printf '  worktrees pruned   : %s\n' "$WORKTREES_PRUNED"
fi
echo "================================================================"
