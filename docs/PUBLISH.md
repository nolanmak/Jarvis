# Public repo & publishing

## Where this code lives

- **Public, canonical, agent-tracked:** `github.com/nolanmak/MyAgentAssistant`
  — the deploy box's `origin`. File content and commit messages carry no PII,
  and there is no `refs/pull/*` baggage (fresh repo). **However, author/committer
  metadata is NOT yet scrubbed:** 16 commits across all refs (14 on `main`) still
  carry one of two personal Gmail addresses in their
  author and/or committer headers, and there is no root `.mailmap` to remap them
  at GitHub's display layer. Scrubbing these to the GitHub noreply
  `119541177+nolanmak@users.noreply.github.com` is a PENDING action tracked in
  issue #358.
- **Archived snapshot (do NOT push to, do NOT publish):**
  `github.com/nolanmak/AugmentAgent` — kept private and archived on GitHub.
  Its branch history is clean, but GitHub's un-rewritable `refs/pull/*` for
  old merged PRs still pin pre-rewrite commits containing the home
  address/phone/emails. **Never flip that repo public.** It exists only as a
  historical issue/PR archive. New issues, PRs, and pushes must go to
  MyAgentAssistant.

## Why a fresh repo (not "make the old one public")

`git filter-repo` + force-push cannot rewrite GitHub's `refs/pull/*`; on a
public repo those old merged-PR diffs would expose PII. A brand-new repo has no
such refs, which is why this repo was created fresh and the agent cut over to
it. That correctly avoids the `refs/pull/*` baggage — but the metadata-scrub
step was **not** completed: the initial push retained the personal-gmail
author/committer headers on `main` and on the published feature/doc branches.
So the public history is not yet guaranteed-clean; the remaining scrub is
tracked in issue #358.

## Keeping it clean going forward

- Personal/business data lives ONLY in gitignored `.env` / sqlite DB / `wiki/`
  — never committed. See `docs/SECURITY.md`.
- Pre-commit guard installed (`scripts/install-git-hooks.sh` →
  `scripts/check-no-personal-data.sh`) blocks secrets/PII in staged content.
- **TODO (tracked in #358):** the deploy checkout's **local** git identity must
  be set to the GitHub noreply so future commits stop re-introducing the personal
  gmail into the public history. This regression is currently live — recent
  commits are still gmail-attributed — so run
  `git config user.email 119541177+nolanmak@users.noreply.github.com`
  (and `git config user.name`) on the deploy box, and don't override it back to
  the gmail. Acceptance: `git log --all --format='%ae %ce' | grep -i gmail` is
  empty.
- There is **no** private→public sync job: there is only one live repo
  (MyAgentAssistant — the agent's `origin`). You push there normally. Do not
  push to nolanmak/AugmentAgent under any circumstance — it is archived.

## Rollback / safety

- Pre-migration full backup: `/tmp/aa-pre-public-20260519-131910.tgz`
  (+ `.bundle`) — complete `.git` + tree restore point taken before any change.
- WIP from prior sessions is preserved in 4 git stashes on the deploy checkout
  (untouched by the migration).
- The author/committer metadata still needs scrubbing (see #358 and the top of
  this doc). To do it, run `git filter-repo --mailmap mailmap.txt` on a fresh
  clone, mapping BOTH personal Gmail addresses (read them from
  `git log --all --format='%ae %ce' | grep -i gmail | sort -u`) →
  `119541177+nolanmak@users.noreply.github.com` for author AND committer, then
  force-push `main` and the affected branches (safe here: no forks/PRs on the
  public repo). As a belt-and-suspenders display fix, also commit a root
  `.mailmap` with the same mappings so GitHub renders the noreply even before the
  rewrite lands. The same `git filter-repo --mailmap` procedure applies if commit
  identity ever regresses to a personal email again.
