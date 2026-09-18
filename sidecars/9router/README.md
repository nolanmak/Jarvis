# Pinned 9Router dependency lock

`package-lock.json` locks dependencies for upstream 9Router commit
`17c4cc76877bd1755030a8414f8d0083f48dcccf` (0.5.75). The installer copies this lock
into that source checkout before `npm ci`; 9Router is built as a separate local
service, not included in the dashboard bundle.

Install, account sign-in, QA and rollback: [model-router.md](../../docs/model-router.md).
