# Atomic integration protection for #1598

This payload is prepared for deployment; adding it to the repository does not
change GitHub settings. Do not enable the new autonomous merge path until the
configured protection has been read back and verified.

GitHub's merge mutation accepts an expected PR head but no expected main head.
Reading main immediately before merging does not close that race. Strict required
status checks, enforced for administrators, prevent an out-of-date candidate from
merging after another change advances main.

The payload requires `privacy` and `node-release` from GitHub Actions app 15368.
Both jobs run for every pull request in `.github/workflows/privacy.yml`, without
path filters. Main's current check receipts confirmed these names and app identity
on 2026-10-26. Conditional jobs must not be made globally required because an
unrelated PR may never produce their check. The controller still requires its
full verification, independent review, exact-head evidence, and other applicable
checks. CodeRabbit and human review are not required by this payload.

During rollout, first reread existing classic protection and effective rules.
Preserve any newly added restrictions; never overwrite concurrent policy changes
with this historical payload. The inspected starting state had no classic branch
protection and only deletion/non-fast-forward rules. After validating the release,
apply the reviewed payload with the GitHub branch-protection API, read it back,
and prove an outdated synthetic PR cannot merge. Update/reverify that candidate
and prove its current approved head can merge. A configuration read failure is an
explicit dependency, not evidence that protection exists.

Record before/after configuration and the rollout's exact code revision in the
deployment receipt. Restore the prior configuration only as part of an explicit
rollback of the corresponding merge behavior; do not disable protection merely
to make a stuck PR merge.
