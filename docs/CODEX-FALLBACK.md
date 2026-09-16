# Codex fallback implementation

Issue #1019 requires operational parity for every Claude-backed Jarvis workflow.
This document tracks the implementation contract. It is not a claim that agentic
fallback is enabled: the production routing gate remains closed until the bridge,
provider conformance tests and deployed QA pass.

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

## Fallback diagnostics

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
through the bridge against allowed-read and denied-write probes. Provider execution/output coverage is
tracked separately as pending; inventory and policy checks alone do not prove
full workflow parity. The remaining provider conformance suite must cover:

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
  capability exclusions do not count as builders. This is instance-local
  attribution; durable draft authorship and independent reviewer selection are
  still required before enabling auto-ship fallback.

## Remaining integration gates

1. Complete writable build/test snapshots, production integration conformance,
   web/document support and precise readiness reporting. Validate all accepted
   settings and tool schemas; never silently discard a required capability.
2. Finish lifecycle verification for the integrated adapter and audit stream,
   including startup failure, timeout, cancellation and descendant cleanup.
3. Add durable handoff accounting for completed and uncertain mutations. Seed the
   fallback with known progress, reconcile uncertain effects, and prevent replay.
4. Complete the machine-checked capability manifest and provider conformance tests.
5. Enable the capability routing matrix only after these contracts pass. Preserve
   independent auto-ship review and all existing merge gates.
6. Verify the deployed query/delivery path and a controlled full auto-ship lifecycle,
   document rollback, merge green reviewed changes and remove task-owned worktrees.

All fixtures and publishable receipts must use synthetic data. Live account
configuration, private correspondence and raw runtime logs stay outside this repo.
