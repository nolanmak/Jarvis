# Main protection rollout proposal

Read-only observation on 2026-10-26: canonical repository `nolanmak/Jarvis`
(origin `MyAgentAssistant` redirects there), legacy main protection returns
404 `Branch not protected`. Active ruleset `25861241`,
`protect-default-branch`, prevents deletion and non-fast-forward updates and
has a repository-admin bypass. It has no required checks or PR reviews.

The publisher requires strict up-to-date required CI checks without an actor
bypass. Existing settings therefore stop new merges with
`merge-dependency:strict-integration-protection`. This is unrelated to
CodeRabbit or any missing external approval.

Proposed change: create legacy main protection using the adjacent
`main-protection.json`. Preserve the existing ruleset unchanged. The added
policy requires `privacy` and `node-release` from GitHub Actions (app15368),
strict up-to-date checks, and admin enforcement. It does not require external
PR approvals, CodeRabbit, or the comment-only Claude workflow. Force pushes
and deletion remain prohibited. Autoship continues checking its complete
mandatory CI set in addition to the server-enforced minimum.

Before applying, reread both effective rules and legacy protection. If legacy
protection now exists, do not blindly PUT this template: preserve existing
required contexts/apps, review requirements, restrictions, and other guards;
reconcile any external-review requirement explicitly with the owner's policy.
Recheck the two named CI contexts actually execute for all intended PR types.
This document is a proposed settings diff, not evidence that settings changed.

After application, read back legacy strict checks/admin enforcement and verify
ruleset25861241 remains unchanged. Run the existing controlled merge-boundary
probe to verify a changed base cannot use stale approval. No remote settings
were written during preparation of this proposal.
