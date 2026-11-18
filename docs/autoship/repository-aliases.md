# Repository renames and controller ownership

Autoship uses one offline repository-identity policy for protected repository
names, push admission, push observation, and controller ownership. GitHub URL
case, SSH/HTTPS spelling, and the optional URL `.git` suffix normalize to the
same owner/repository. The Git transport is still pinned to the exact resolved
push URL; alias matching does not permit a hook or URL rewrite to substitute
another destination.

An operator can declare a verified rename in the daemon/worker environment:

```dotenv
AUGMENTAGENT_REPOSITORY_ALIASES='{"fixture-owner/current":["https://github.com/fixture-owner/Before.git"]}'
```

The JSON object maps a stable canonical owner/repository to exact former GitHub
URLs. Each listed URL also authorizes equivalent SSH/HTTPS, case and `.git`
spellings of that former name. An unlisted repository stays a different
repository. Local mirrors retain their existing exact-path receipt behavior.
Malformed, ambiguous, or chained alias declarations fail closed. Aliases are
operator authorization: verify that both names identify the same GitHub
repository before configuring them. Candidate Git config, issue bodies, and
model tool options cannot grant aliases. No lookup is added to the pre-push hook.

## Existing controller receipts

Version 1 receipts contain a SHA-256 of the exact origin URL, not the URL text.
Autoship recognizes a legacy hash only from the current URL, an explicitly
listed exact former URL, or conventional canonical URL spellings. When a URL
has already changed, list the exact historical spelling, including its case
and suffix; an unknown hash cannot safely be reversed or guessed.

Under the existing per-controller lock, ordinary preparation verifies the same
primary repository, directory device/inode and Git pointer bytes. It still
requires a clean detached controller and refuses unmerged controller work.
Only after successful preparation and a final identity recheck does it
atomically write a private version 2 receipt. The origin witness now hashes
`github:<canonical-owner/repository>` and retains the prior URL hash in
`previous_origin_sha256`. A later explicitly configured rename of an existing
v2 identity also retains its prior identity hash. The migration is logged.
Unrecognized hashes, physical replacement, missing receipts, dirty work, and
genuinely different repositories do not migrate.

No workflow checkout receipt, candidate binding, acceptance evidence, owner
pause, push intent, or remote ref is reset by this migration. Controller
ownership remains distinct from candidate checkout continuity.

## Rollout order

1. Set the verified alias mapping in the operator configuration and load it in
   the updated worker through the normal deployment/restart process. Both the
   serving process and a separate autoship worker must receive the same policy.
2. Leave origin unchanged for the first updated controller preparations if
   convenient. The known v1 receipts migrate lazily as their lanes run.
3. Origin may then be changed to the canonical URL using normal Git remote
   configuration. Known legacy receipts can also migrate after the URL change
   if the mapping contains their exact historical URL.
4. Keep aliases while any worker still uses the old URL or needs legacy-hash
   migration. Once all URLs are canonical and receipts have migrated, the
   historical alias can be removed; canonical v2 receipts remain valid.

Do not delete ownership receipts to get past a refusal. A failed migration
preserves the original receipt for diagnosis. Migrated receipts require this
version-aware binary; a rollback to a v1-only binary will refuse them rather
than silently changing ownership.
