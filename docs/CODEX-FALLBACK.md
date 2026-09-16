# Codex fallback implementation

Issue #1019 requires operational parity for every Claude-backed Jarvis workflow.
This document tracks the implementation contract. The development branch now
permits Codex routing for text, read, write and full-agentic requests through the
scoped bridge. This has not been deployed and is not a claim of full workflow
parity: remaining provider conformance and operational QA still gate release.

## Execution boundary

The model runs in a fresh empty directory with project configuration discovery
excluded, native shell/apps/plugins/browser/computer/image tools disabled, and a
named minimal-read/no-write/no-command-network permission profile. Provider login
remains in the existing adapter; integration credentials belong to private bridge
configuration, never command-line arguments or model-visible policy text.

A required stdio MCP bridge exposes the operations declared by `ReasonerOpts`.
The bridge enforces tool identity, workspace scope, argv validation and existing
pre-tool guards. Every filesystem path component is opened without following
symlinks. Read roots and write roots are separate; transcript context is not a
writable workspace. Credential and control directories are excluded.

The original guards run inside the bridge and fail closed on crash, timeout,
malformed output or explicit denial. Native Codex hooks are not the enforcement
boundary: a live synthetic probe found that a crashing hook allowed an MCP call
to continue. Another live probe confirmed that the restricted native permission
profile rejected an edit while the bridge successfully read and wrote a synthetic
file, preserving its bytes.

The bridge tracks its parent process with a Linux pidfd. A live builder probe
exposed that the previous parent-death signal was tied to Codex's launching
thread: when that thread retired, the bridge exited while Codex remained alive.
A deterministic regression reproduces that failure and now passes. A companion
test keeps stdin open after the parent exits and verifies the bridge still
terminates, retaining the parent-process lifecycle boundary.

Commands must use parsed argv, never a model-generated shell script. Matching a
command prefix alone does not sandbox programs such as Cargo or npm: they can
execute project code. Build execution therefore needs a separately verified
filesystem/process/network boundary. The command helper now enforces Landlock
ABI 6+ read/write scopes plus a seccomp deny list for networking, process
introspection and escape from the cleanup process group. Source-file read grants
use opened inodes and exclude credential/control paths. The current general
command profile is read-only. Cargo/npm commands run in disposable source
snapshots with separate temporary configuration and output directories. Existing
Cargo caches, Rust toolchains and npm dependencies are read-only; missing
dependencies cannot currently be downloaded. UTF-8 source changes pass through
the original write guards and concurrent-edit checks before being copied back.
Build artifacts, binary changes and deletions are not copied back. Real synthetic
Cargo and npm tests verify execution; dependency provisioning, cache persistence,
binary/deletion reconciliation and full project builds remain integration gates.
An actual `cargo check -p augmentagent-channel-core --offline -j 2` completed
inside the build sandbox, compiling the real dependency tree. Running the core
unit suite inside it exposed further gaps: local HTTP fixtures cannot open sockets,
process supervision tests cannot create sessions/groups, and the sanitized HOME
environment changes one path-redaction fixture. That run had 340 passes, 22
failures (including the expected routing regression), and one ignored live test.
These failures are outstanding build parity work, not grounds to skip the tests
or globally remove confinement.
The HOME identity is now preserved as an OS environment value; a focused
regression confirms that this still grants no read access to files in that home.
The full sandboxed core suite has not yet been rerun after that correction.
Git inspection receives read-only repository metadata and disables external diff
helpers, hooks, fsmonitor and user/system configuration. On the tested deployment, the default Codex
command sandbox fails during loopback setup. Its legacy Landlock backend runs a
simple command but rejects permission profiles requiring direct runtime
enforcement; selecting that backend alone does not prove read confinement.
Additional host probes found no installed Docker/Podman runtime. A user-service
`PrivateNetwork=yes` probe exited successfully but retained the host network
namespace, including with `PrivateUsers=yes`; direct user/network namespace
creation was denied. An exit status alone is therefore not evidence of network
isolation. No host security setting was changed during these probes.

A subsequent host probe verified KVM API access and creation of a VM by the
service account. A private QEMU runtime extracted from the configured Ubuntu
package archive booted a matching kernel with a synthetic initramfs, no network
device, and QEMU's seccomp sandbox enabled. Inside the guest, loopback socket I/O
and a detached process session both worked. A synthetic read-only 9p share was
readable; attempted writes failed, and a symlink to a host file outside the share
could not be read. The guest exposed only `lo` and powered off successfully.
These probes establish a possible local build runner without administrative
setup, not complete build parity. Runtime provisioning, scoped build/cache shares,
resource limits, cancellation, source reconciliation and actual project-suite QA
must be integrated before replacing the current command sandbox. QEMU device,
share and sandbox options follow its [invocation reference](https://www.qemu.org/docs/master/system/qemu-manpage.html).

The prototype `scripts/codex-build-vm.py` now executes a disposable snapshot in
that guest. The workload runs as an unprivileged UID with no-new-privileges;
runtime and dependency mounts are read-only and disable setuid/device semantics.
A root-only guest control directory separates the command result from workload
output. The host waits for VM exit and the supervisor cleanup receipt before
accepting it. Real tests cover scoped writes, blocked host/symlink access, absence
of daemon environment secrets, read-only cache mounts, receipt-forgery refusal,
exit codes and cancellation after a detached child has demonstrably started.
A Cargo fixture compiles and runs socket/session tests in the guest. Running
the actual core unit suite from a fresh snapshot compiled in 2m26s and produced
376 passes, three ignored live tests, and only the existing full-agentic routing
regression failure. The earlier socket, session and HOME failures did not recur.
Bridge Cargo/npm/npx commands use this runner when the private default runtime
configuration exists, or the operator sets `AUGMENTAGENT_BUILD_VM_CONFIG`. The
adapter reads the override from its own process environment, never from profile
environment overrides. See [runtime setup and rollback](BUILD-VM.md). The launcher bundles both
the VM helper and process supervisor privately. Checkout path arguments translate
to the guest workspace, guest tools do not depend on the host command PATH, and
root npm dependencies mount read-only. Source reconciliation runs only after VM
shutdown and retains original Write hooks and concurrent-edit checks. A real
bridge test verifies npm dependency loading, loopback, process sessions, source
updates and guard denial. Live Codex QA ran a Cargo socket test through the
packaged bridge, verified its successful tool audit and confirmed build outputs
stayed out of the source worktree. A durable private runtime was provisioned with
a package/version/hash record; a second live test passed using default discovery
without an environment override. This did not restart or deploy the daemon. Cache
reuse, missing dependency provisioning, nested npm workspaces, resource policy
and binary/deletion reconciliation remain integration work. Real-VM Python tests
require `JARVIS_TEST_VM_CONFIG` pointing to owner-private runtime configuration;
the live adapter test uses default discovery or the daemon override.

## Fallback diagnostics

Doctor reports routing capacity separately for text, read, write and agentic
workloads. Each configured provider is identified as a candidate, capability
excluded, on cooldown, or unavailable at binary/auth preflight. A single candidate
warns that no backup remains; zero candidates is an error. This does not claim
MCP, guard or sandbox readiness, which still needs validation for the concrete
invocation. The existing chain finding retains detailed binary/auth explanations.

Exhausted-chain errors now distinguish capability exclusion, active cooldown,
attempted quota failure, timeout, provider unavailability, local readiness failure
and CLI-gate timeout for the entries in the chain. The display contains only
provider names and failure categories. The original typed provider error remains
in the error chain for existing cooldown/retry callers. Constructor-time exclusions
and detailed binary/auth/sandbox readiness still require integration with status
and doctor output.

## Handoff journal implementation status

The bridge accepts an optional owner-private operation journal outside all model
file scopes. It persists a started receipt before an external tool call and a
completed receipt only after a successful result, with file and directory fsync
and an exclusive execution lock. A restarted bridge returns the stored result
for identical completed tool arguments. Uncertain outcomes, including reported
tool errors, block further external effects until reconciliation. Local reads
remain fresh. Known read-only Gmail, repository-document, GitHub inspection and
guarded SocialAPI operations also bypass mutation receipts so they can gather
current evidence during reconciliation. Unknown operation contracts remain
potentially mutating; server advisory annotations alone do not exempt a tool.
An uncertain write returns a safe, actionable tool error without its arguments.
Tests cover restart, ambiguous connection failure, corrupt and
symlink state, model-scope exclusion and actual bridge receipt reuse.

Production dispatch now assigns private journal paths for write/agentic calls and
forwards recorded progress to the next provider. Requests with a channel turn id
have a stable hashed identity across restart; callers without an id get distinct
journals and still need caller-owned restart identity. Claude receives pre-tool,
post-tool and failed-tool hooks using the documented [hook event contract](https://code.claude.com/docs/en/hooks).
The pre-tool hook persists started state; successful post-tool events normalize
results for Codex. The generated command turns script startup failures into exit
2. Tests execute that command, including quoted paths and blocking results, and
exercise receipt forwarding through the dispatcher.

This is not completed cross-provider handoff. Live primary hook conformance,
effect-aware read/reconciliation operations, retention, caller identity coverage,
intentional repeated-action semantics and interruption cleanup remain gates.
Exact argument matching does not identify semantically duplicate actions expressed
with different arguments. Hook observations do not substitute for verified
termination of the previous provider and its descendants before handoff.

Both CLI adapters now launch beneath a private Linux subreaper supervisor. On
normal exit or cancellation it kills the provider group, adopts detached orphan
descendants, and acknowledges cleanup only after reaping all children. Cancellation
waits for that acknowledgment before releasing the adapter call. Missing cleanup
confirmation produces `CleanupUncertain`, which blocks fallback without latching
the provider. Tests first reproduced group-only and detached-session leaks, then
verified cancellation, normal exit with background work, destruction of the
supervisor itself, and the fallback exclusion. A live Codex read/write/command
smoke test passes through the supervisor. Journal-backed invocations now create
an owner-private, fsynced active-request marker before spawning. Verified cleanup
retires it; missing cleanup confirmation preserves it. The dispatcher checks
this marker before reading any operation receipts, so a restarted request cannot
silently retry, even if the crash preceded the first tool call. The marker also
prevents concurrent invocation of the same request. Tests cover exclusivity,
normal retirement and persistence after supervisor destruction. After a daemon
crash, recovery accepts only an owner-private receipt confirming all descendants
were reaped. A lifecycle lock and invocation-specific receipt path keep an older
invocation from retiring a newer invocation's marker. Tests kill a real parent
process and verify detached work stops before recovery. Callers without stable
turn identities remain integration work; markers are never cleared by age.

## Capability inventory

The checked-in [capability manifest](reasoner-capabilities.json) currently records
31 production constructors, wrappers and policy-changing call sites. A Rust AST
inventory test checks it against source, including code after test modules,
opt-in wrappers and tool-list mutations. Parameterized tests construct twelve
core presets and check capability classification, declared tools and bridge write
scope, including optional wiki access. Isolated opt-in integration tests cover
both wrapper constructors with the feature disabled and enabled, preserving
private authentication configuration and executing the original read-only guard
through the bridge against allowed-read and denied-write probes. All twelve core
presets also execute file operations through the packaged stdio bridge: scoped
Read/Glob/Grep, permitted Write/Edit, read-only mutation denial and outside-root
read/write denial. These deterministic probes cover file tools; they do not
substitute for MCP initialization or model output contracts. Provider execution/output coverage is
tracked separately as pending; inventory and policy checks alone do not prove
full workflow parity.

The live `live_wiki_query_profile_executes_files_and_memory_mcp` test uses the
production query instructions, tool inventory and scope hook, with a synthetic
wiki/database and a built memory server selected by `JARVIS_TEST_MEMORY_BIN`.
It exposed an existing memory-server notification bug: an unsolicited response
to `notifications/initialized` broke the bridge handshake. The server now ignores
notifications and does not execute id-less tool calls; a real stdio regression
test covers both. After that fix, live Codex completed Glob/Grep/Read/Write/Edit
and `memory_recent`, with successful provider-attributed audit records. One
earlier post-fix call initialized but did not produce the requested file; the
test now includes the synthetic response/audit when that assertion fails.
This direct-adapter receipt does not establish reliability across all output
contracts; automatic fallback and handler coverage are tested separately below.

The deterministic `query_handler_preserves_context_and_original_attachment_bytes`
contract calls the real `WikiQuerier::answer` and Discord attachment preparation
with a stubbed primary provider. It verifies the production full-agentic profile,
scope hook, restricted environment, session context, owner rules, transcript
capture and original binary bytes. The live
`live_query_fallback_delivers_original_and_skips_latched_primary` contract now
passes through that handler with a synthetic quota-refusing primary and real
Codex. Two successive requests preserve original PDF bytes, use scoped Read and
the real memory MCP server, and record Codex as the serving provider. The second
request skips the latched primary. These are local handler/delivery-preparation
checks, not a Discord network send or deployed-daemon receipt. Outbound attachment
reads now use a pinned wiki directory
descriptor and reject symlinks at every subsequent path component; metadata and
the byte cap are checked on the opened file. Tests cover swaps after marker
validation, root replacement, oversized files and nonregular files including
FIFOs, without reopening the validated absolute path for delivery.

Routing regressions now cover all four capability classes with both a healthy
primary and a quota-refusing primary, including a second request during cooldown.
The previously failing production-shaped wiki-ask regression passes. The full
core unit suite reports 379 passes and five ignored live tests with routing
enabled; live tests are run explicitly for the receipts described above.

The live `live_codex_fallback_builder_fixes_code_and_runs_red_green_tests` fixture
uses auto-ship's actual `fix_opts` prompt and tools, a latched primary, private
handoff state and real Codex. It observes a failing Cargo regression, edits the
source, passes the same unchanged acceptance test, and successfully inspects
`git diff`. Cargo executes inside the VM; no target directory is produced in the
source checkout. The test now uses the production chain constructor and persists
builder history, then reloads it through a fresh reasoner instance. Codex remains
the sole recorded builder and is excluded from review. With Claude still latched,
the independent review is unavailable and cannot approve. After clearing only the
synthetic test cooldown, real Claude completes and approves the focused-diff and
system-interaction passes. That approval does not grant Codex-specific merge
overrides. This proves the builder and reviewer slices; the full issue/PR/merge/
deployment lifecycle remains a separate acceptance requirement.

The remaining provider conformance suite must cover:

| Source | Presets / operations |
|---|---|
| `channel-core/reasoner.rs` | `triage_opts`, `draft_opts`, `lint_opts`, `ask_opts`, `digest_opts`, `tone_summarize_opts`, `social_adapter_opts`, `loop_parse_opts`, `archetype_pick_opts`, `ingest_opts`, `wiki_migrate_opts`, `resume_opts` |
| `channel-core/reasoner.rs`, `mcp.rs` | `socialapi_draft_opts`, `with_socialapi_readonly_mcp`, configured stdio/HTTP MCP additions |
| `channel-core/resolve.rs` | `extract_opts` |
| `cli/self_improve.rs` | `scope_opts`, `review_opts`, `fix_opts`, `codex_review_opts` |
| `cli/main.rs` | text-only reasoner selftest and production query dispatch |
| channel crates | email signature extraction, journal composition, voice extraction, email/LinkedIn/WhatsApp/Slack/Instagram/Twitter/Discord parsing calls |

Output parity includes model tier selection, code-mode parsing, images, last
assistant block versus complete transcript, original attachment markers, tool
audit records, cancellation and shared CLI-gate lifecycle. A text-only selftest
cannot stand in for any tool-using profile.

## Current verification

- Production-shaped wiki-ask quota regression reproduces the existing rejection;
  it intentionally remains red until agentic routing and enforcement are ready.
- Bridge tests cover file read/write/edit, nested writes, bounded search, tool
  declaration, traversal and intermediate symlink escapes, sensitive paths,
  command parsing, and guard denial/crash/malformed-response handling.
- File-tool schemas and dispatch support optional line ranges, scoped/file
  searches, case-insensitive matching, explicit replace-all edits and bounded
  command timeouts. Invalid or unknown local arguments are rejected before
  guards and execution instead of silently being ignored.
- Recognized but unimplemented local tools fail readiness instead of silently
  disappearing from the advertised tool list.
- Scoped `Read` returns PNG/JPEG/GIF/WebP bytes as MCP image content rather
  than attempting UTF-8 decoding. Path checks and file-size limits apply before
  encoding; text line ranges on images are rejected. A live Codex test reads
  a synthetic PNG through the bridge, identifies its undisclosed color, and
  verifies the original bytes and provider audit record.
- PDF `Read` supports explicit page numbers/ranges and returns rendered page
  images, including visual content. Poppler (`pdfinfo` and `pdftoppm`) renders
  only a private snapshot under the command sandbox, with no network, a 512 MiB
  address-space limit, bounded files and a 60-second request budget. Up to 20
  pages are allowed per call; larger documents require an explicit range.
  A live Codex test identifies the undisclosed color on the selected page of a
  synthetic two-page PDF and verifies the source bytes remain unchanged. Other
  document formats and the complete query/delivery path remain integration work.
- Rust launch tests check private configuration permissions, exclusion of secrets
  from arguments, native tool restrictions, separate read/write roots and
  rejection of unknown settings.
- Stdio and HTTP MCP tests cover tool allowlists, session/auth forwarding,
  environment interpolation, missing-tool readiness and hung-child cleanup.
- Kernel sandbox tests verify scoped I/O, outside/symlink/credential-read denial,
  blocked network sockets and blocked signals to the parent.
- Real Cargo and npm fixtures verify compilation/test execution, read-only npm
  dependencies, source reconciliation and exclusion of build outputs. Git diff
  verifies repository metadata access without granting metadata writes.
- The live Codex adapter smoke test performs scoped reads, exact writes and an
  allowed command; the common audit log verifies the serving provider and exit
  status. This is still not full chat, integration or auto-ship parity.
- The dispatcher records every provider attempted with mutation-capable tools,
  including failed and cancelled calls. Text-only calls, cooldown skips and
  capability exclusions do not count as builders. Auto-ship binds a private,
  durable history per repository and branch before invoking any builder. History
  writes must succeed before a provider runs; restarting or opening a fresh
  attempt cannot erase earlier authors. Missing legacy history remains unknown.
- Independent review selects Codex or Claude only when that provider is absent
  from the draft's complete builder history. Unknown/corrupt history or no
  independent capacity blocks approval. Claude reviews use an explicit model
  pin and the same two-pass evidence contract. Codex-specific owner overrides
  for hard complexity and runtime receipts still require actual Codex approval.
  Live reviewer parity and the complete auto-ship lifecycle remain unverified.

## Remaining integration gates

1. Complete writable build/test snapshots, production integration conformance,
   web/document support and precise readiness reporting. Validate all accepted
   settings and tool schemas; never silently discard a required capability.
2. Finish lifecycle verification for the integrated adapter and audit stream,
   including startup failure, timeout, cancellation and descendant cleanup.
3. Add durable handoff accounting for completed and uncertain mutations. Seed the
   fallback with known progress, reconcile uncertain effects, and prevent replay.
4. Complete the machine-checked capability manifest and provider conformance tests.
5. Release the expanded capability routing only after these contracts pass.
   Development routing is enabled for integration QA; preserve independent
   auto-ship review and all existing merge gates before deployment.
6. Verify the deployed query/delivery path and a controlled full auto-ship lifecycle,
   document rollback, merge green reviewed changes and remove task-owned worktrees.

All fixtures and publishable receipts must use synthetic data. Live account
configuration, private correspondence and raw runtime logs stay outside this repo.
