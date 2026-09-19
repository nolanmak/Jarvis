# Pinned 9Router dependency lock

`package-lock.json` locks dependencies for upstream 9Router commit
`17c4cc76877bd1755030a8414f8d0083f48dcccf` (0.5.75). The installer copies this lock
into that source checkout before `npm ci`; 9Router is built as a separate local
service, not included in the dashboard bundle.

Install, account sign-in, QA and rollback: [model-router.md](../../docs/model-router.md).

`runpod-reconciliation.patch` applies to the same pinned upstream commit. It
forwards a caller's `Idempotency-Key` through OpenAI-compatible nodes and keeps
an upstream 409 as 409, so the caller can reconcile a possibly accepted job.
It returns 422 for an unsupported current or historical attachment on an explicit
OpenAI-compatible model instead of silently removing that input.
Explicit compatible-node requests bypass 9Router's capacity adapter, so a
model selection cannot silently send the request to another provider.
`scripts/install-model-router.py` applies it before the Linux build.
`scripts/build-model-router-image.sh` produces a Docker image with this patch
and the committed dependency lock for a Docker-hosted router.
