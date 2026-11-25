# Explicit PR readoption

An authorized owner can readopt an existing same-repository PR, regardless of
its branch name or whether it has a source issue:

```sh
augmentagent autopr readopt --repo OWNER/REPO --pr 42 \
  --reason 'Request current independent review' --request-id owner-pr-42-v1 --json
```

In the configured owner's private Discord conversation:

```text
!autoship readopt https://github.com/OWNER/REPO/pull/42 --reason Request current independent review
```

Both paths authorize the current GitHub identity and resolve the current PR
before writing state. Forks, closed PRs, disallowed repositories and unauthorized
authors are refused. Readoption records the current head, base, branch, source
link, author, origin and owner reason, then joins the shared command workflow.
CLI request IDs and Discord message identities make redelivery idempotent.
Changing a request's purpose requires a new request ID.

Acceptance means durable intake, not execution, review approval or completion.
Readoption preserves owner pauses, security holds, active leases and prior
workflow evidence. It does not dispatch models or clear GitHub labels. Normal
execution still resolves the current head and requires independent review,
current CI and applicable merge policy. A stacked base remains subject to the
existing integration dependency checks.
