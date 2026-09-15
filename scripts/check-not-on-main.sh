#!/usr/bin/env bash
# check-not-on-main.sh — refuse a commit made directly on the deploy branch.
#
# The auto-updater deploys by fast-forwarding this checkout to origin/main. A
# commit made here on `main` cannot fast-forward once the PR is squash-merged
# upstream: local and origin diverge, the updater refuses to pull, and it
# stalls SILENTLY on the old commit — the daemon keeps running stale code
# until a human notices. That happened on 2026-09-14 and had happened before.
#
# Work on a branch instead; the PR flow is what puts code on main.
#
#   git switch -c fix/my-change     # then commit as usual
#
# Bypass for a genuine one-off (a hotfix you will push straight to main):
#   git commit --no-verify
set -uo pipefail
cd "$(git rev-parse --show-toplevel 2>/dev/null || echo .)"

# Only guards the deploy checkout. Worktrees (the self-improve loop's
# `agent-fix/*`, review worktrees) have their own HEAD and are unaffected.
branch=$(git symbolic-ref --quiet --short HEAD 2>/dev/null || echo "")
case "$branch" in
  main|master) ;;
  *) exit 0 ;;
esac

cat >&2 <<MSG

✗ Refusing to commit directly on '$branch'.

  This checkout is what the auto-updater deploys from. A commit here diverges
  from origin/main the moment the PR is squash-merged, and the updater then
  stalls silently on the old commit.

  Do this instead:
      git switch -c <branch-name>
      git commit ...            # your staged changes are still staged
      git push -u origin HEAD

  Genuine exception (you will push straight to main): git commit --no-verify
MSG
exit 1
