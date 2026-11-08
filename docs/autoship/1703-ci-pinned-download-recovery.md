# Pinned CI download recovery

This change addresses dependency setup failures observed on recovery PR #1758,
head 38cf3e004720a0eaba50cb2058114cd8cdb2f0ab, before its relevant tests ran:
Python 3.9 exporter job 128702059684, privacy job 128702059991, targeted job
128702059762, and CLI job 128702060327. The release downloads returned HTTP 504.

Deno remains 2.7.14, gitleaks remains 8.30.1, and Python compatibility coverage
remains CPython 3.9 (now exactly 3.9.25). Their immutable archive SHA-256 values
match the official release asset digests. Deno tries its official CDN and GitHub;
Python and gitleaks try their public GitHub asset API and release URLs. No token
is supplied. The routes have a shared availability risk: this is bounded
transport recovery, not an independent mirror for a complete GitHub outage.

Each route is attempted at most twice, within a shared 120-second download
budget and a 30-second socket timeout (an in-flight socket may exceed the budget
by up to that timeout). Downloads have a 128 MiB size limit. Checksums are
verified before extraction or execution. No release setup script runs; in
particular Python's bundled setup script would perform an unpinned pip upgrade.
The pinned setup actions still select completed runner tool-cache entries.
Existing completed entries retain those actions' normal runner-cache trust.
Incomplete entries are refused without replacement. A failed binary smoke test
cannot publish a tool-cache completion marker. Provisioning is explicitly held
in the local VM CI contract until controlled provisioning exists; tests are not
silently marked passed or skipped. Required suites and merge signals are unchanged.

Validation on 2026-11-08:

- 60 CI contract, planner, storage, VM and pinned-download regression tests pass.
- All three real, previously downloaded hash-pinned archives pass installation
  and executable smoke tests in a disposable simulated runner cache.
- That CPython 3.9.25 passes all 91 Apple Notes exporter tests.
- A fresh real HTTPS run downloaded Deno successfully, but Python's API returned
  503 and its release URL timed out. A separate curl API check returned 504.
  No Python completion marker was published. Hosted success remains unproven.
- Affected execution workflows use read-only contents permission, checkout
  without persisted credentials, and contain no secret references or
  pull_request_target triggers. No credentials or self-hosted runner are added.

This change does not deploy the recovery worker, merge #1758, qualify historical
native resources, prove independent exact-head review, or satisfy the live
CLI/Discord, autonomous merge and soak requirements of #1703.
