# Public repo & publishing

## Where this code lives

- **Public, canonical, agent-tracked:** `github.com/nolanmak/MyAgentAssistant`
  — the deploy box's `origin`. History is scrubbed: no PII in file content,
  commit messages, or author/committer metadata (all attributed to the GitHub
  noreply `119541177+nolanmak@users.noreply.github.com`). No `refs/pull/*`
  baggage (fresh repo).
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
such refs, so pushing only the scrubbed `main` yields a guaranteed-clean public
history. This repo was created that way and the agent was cut over to it.

## Keeping it clean going forward

- Personal/business data lives ONLY in gitignored `.env` / sqlite DB / `wiki/`
  — never committed. See `docs/SECURITY.md`.
- Pre-commit guard installed (`scripts/install-git-hooks.sh` →
  `scripts/check-no-personal-data.sh`) blocks secrets/PII in staged content.
- The deploy checkout's **local** git identity is set to the GitHub noreply
  (`git config user.email`), so future commits never re-introduce the personal
  gmail into the public history. Don't override it with the gmail.
- There is **no** private→public sync job: there is only one live repo
  (MyAgentAssistant — the agent's `origin`). You push there normally. Do not
  push to nolanmak/AugmentAgent under any circumstance — it is archived.

## Rollback / safety

- Pre-migration full backup: `/tmp/aa-pre-public-20260519-131910.tgz`
  (+ `.bundle`) — complete `.git` + tree restore point taken before any change.
- WIP from prior sessions is preserved in 4 git stashes on the deploy checkout
  (untouched by the migration).
- If commit identity ever regresses to a personal email, fix with
  `git filter-repo --mailmap` (map gmail → the noreply) on a fresh clone, then
  force-push (safe here: no forks/PRs on the public repo).
