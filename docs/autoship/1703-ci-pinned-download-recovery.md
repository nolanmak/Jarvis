# Pinned CI download recovery

Release-download HTTP 504s blocked recovery PR #1758 at head
38cf3e004720a0eaba50cb2058114cd8cdb2f0ab before relevant tests ran: exporter
128702059684, privacy 128702059991, targeted 128702059762, CLI 128702060327.

Deno remains 2.7.14, gitleaks remains 8.30.1, and Python compatibility coverage
remains CPython 3.9 (now exactly 3.9.25). Binary SHA-256 pins match official
release asset digests. Deno tries its official CDN and GitHub; Python and
gitleaks try the public GitHub asset API and release URLs. No token is supplied.
A download has two attempts per route, a shared 120-second budget, a 30-second
socket timeout and a 128 MiB size limit. An in-flight socket can exceed the
budget by up to that timeout. Hashing precedes extraction or execution.

Real hosted results on #1909 head fc0150ca proved that API/release routes can
fail together: Apple Notes exporter 128707515929 and privacy 128707469904
exhausted them with 503/504/timeouts. The targeted job 128707471585 successfully
provisioned Deno, then correctly refused those failed dependencies. iMessage
exporter 128707515358 passed. These failures were not test skips or successes.

The binary routes therefore have independent source fallbacks:

- CPython comes from the [PSF 3.9.25 source release](https://www.python.org/downloads/release/python-3925/).
  SHA-256 `00e07d7c0f2f0cc002432d1ee84d2a40dae404a99303e3f97701c10966c91834`
  matches the release's official Sigstore message digest. The Sigstore signature
  itself was not verified here. Configure disables ensurepip; make uses two jobs
  without PGO or debug info. The declared GitHub job first installs required compiler-library headers
  from Ubuntu's signed repositories (bzip2, ffi, lzma, sqlite, SSL and zlib).
  This setup and provisioning share a five-minute step limit. No pip upgrade
  or bundled binary-release setup script runs. SSL, sqlite, compression and ctypes imports
  must pass before the setup-action cache completion marker is written.
- Gitleaks comes from the immutable public Go module proxy ZIP, with SHA-256
  `4fd70631c0f6159e38fdb99ad126e3ba60d4212420062029d3dc5f510f6f2961`.
  Its Go module content hash matches the public sum.golang.org record
  `h1:PmEvCfVI7ti9dV3s5aMZUY7sS2GxRvG3yzih7E+cS3w=`. Dependencies keep go.sum
  and public checksum DB verification; direct GitHub fetching and implicit
  toolchain downloads are disabled. The privacy workflow supplies pinned Go
  1.27.1. Build uses two jobs, the release's gore2regex backend and version linker
  variable. The resulting scanner must report exactly 8.30.1 before publication.

Compilation has separate bounded subprocess timeouts (Python configure 180s,
make 600s, install 180s; Go build 600s), and each source download has its own
bounded budget. Source-built binaries are not claimed byte-identical to the
release archives. They preserve the interpreter/scanner versions and tests.
All work occurs in the isolated hosted job and disposable staging paths.

Pinned setup actions still select completed tool-cache entries; Python's
AGENT_TOOLSDIRECTORY precedence is preserved. Existing completed entries retain
those actions' ordinary runner-cache trust. Incomplete entries are refused
without replacement. Failed smoke tests cannot publish completion markers.
Provisioning remains explicitly held in the local VM CI contract until its
controlled provisioning exists. No required suite or merge signal is waived.

Local validation on 2026-11-08:

- 66 CI contract/planner/storage/VM/download regression tests pass, including
  RED-to-GREEN tests that binary outages reach independent sources and corrupt
  source bytes never reach the compiler or publish a completed installation.
- All three real pinned binary archives pass installation/executable smoke
  tests in a disposable simulated runner cache. Binary Python passes 91 Apple
  Notes tests.
- Real PSF source download, checksum verification, build and extension smoke
  pass in 47.17s; that interpreter passes 91 Apple Notes and 102 iMessage tests.
- Real public Go module download, checksum verification and Go 1.27.1 build pass
  in 24.81s. Source and release scanners agree on clean and synthetic-token
  fixtures, and the source scanner reports no leaks across the working tree.
- A subsequent real source-fallback replay on #1758 head 0d636a2c (exporter
  job 128723431050) exposed missing `_bz2` after both GitHub routes failed.
  The import smoke check correctly refused the incomplete interpreter and no
  completion marker was published. The job now declares all required build
  headers; the new regression failed before that declaration and passes after.
  System dependency versions follow the runner's signed Ubuntu repositories;
  immutable binary/source artifact pins and smoke checks remain mandatory.
- The exporter job now explicitly runs `python39 --verify-source` in a private
  hosted prefix, after its required compatibility tests. This compiles the
  pinned PSF source and checks the complete required extension set even when
  binary downloads or tool-cache lookup succeed. It has the same five-minute
  step bound and explicit controlled-provisioning declaration; local host setup
  is refused. Its first hosted result remains unproven.
- Final exact-head hosted results with header provisioning remain pending; earlier hosted results above
  are evidence of the missing fallback, not approval of the final change.
- Affected execution workflows use read-only contents permission and checkout
  without persisted credentials. They contain no secret references or
  pull_request_target triggers. No credentials or self-hosted runner are added.

This does not deploy the recovery worker, merge #1758, qualify historical native
resources, prove independent exact-head review, or satisfy the live CLI/Discord,
autonomous merge and soak requirements of #1703.
