# Public repository and release checklist

The public project is <https://github.com/nolanmak/Jarvis>. Existing deployment
checkouts may still use a remote named `origin` pointing at MyAgentAssistant.
Do not change deployment remotes, rewrite history, or restart services as part
of a source-only release cleanup. The updater follows `origin/main`.

## Source checks

Run from a clean checkout with dependencies installed:

```sh
bash scripts/check-no-personal-data.sh --tracked
python3 scripts/tests/check-no-personal-data.test.py
npm run build
npm test
cargo test --workspace
```

Use reserved `example.com`, `example.net`, `example.org`, `.example`, or `.test`
domains in examples, including their subdomains. Keep test inputs and expected
values consistent. `pii-ok` is only for reviewed synthetic credentials or
service identifiers whose format is necessary to a test; it must never exempt
a real person's address, a copied message, or a credential.

The privacy workflow checks the tracked tree and scans an archive of that tree
with Gitleaks. Templates are scanned too. A green check is a backstop, not a
complete privacy or application-security audit.

## Historical exposure is a separate release gate

Deleting an address in a new commit does not remove it from earlier commits,
branches, pull-request diffs, issue comments, Actions logs, or existing clones.
The September 2026 audit found personal email metadata in 14 commits reachable
from the audited `main`, and personal-looking addresses in source examples.
The remote also has older branches and GitHub-managed pull-request refs.

Before promoting the repository:

- Scan all branches and tags, commit metadata, and GitHub-managed PR refs.
- Review issue/PR bodies, comments, and published artifacts for copied private
  context. Do not attach unredacted audit reports to public issues.
- Coordinate a history rewrite with deployment checkouts and collaborators.
  GitHub-managed PR refs cannot be cleaned with an ordinary branch force-push;
  retained objects may require GitHub assistance. Alternatively, publish a
  fresh repository from a reviewed source archive and keep the old repository
  private. Neither option recalls existing third-party copies.
- Rotate any credential confirmed to have been exposed. The initial Gitleaks
  audit found synthetic examples, not confirmed live credentials.
- Resolve the grocery-provider permission check in
  [third-party notices](../THIRD_PARTY_NOTICES.md).

The private AugmentAgent repository is a historical archive and must stay
private. Do not assume it is protected merely because an old document called
it archived; verify the actual GitHub settings before any migration.

## Packaging

Build a release archive from the reviewed commit with `git archive`, never
by zipping a running deployment directory. This excludes ignored `.env`
backups, databases, session cookies, and private wiki content. Only the empty
grocery wiki scaffold belongs in the release.

No version tag or public release should be published until the source checks,
historical-data review, and third-party permission check are complete.
