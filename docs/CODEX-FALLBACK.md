# Codex fallback implementation

Issue #1019 requires operational parity for every Claude-backed Jarvis workflow.
The implementation merged in PR #1021 at `92dcaa9` and is deployed. Codex handles
text, read, write and full-agentic requests through an enforced scoped bridge.
The capability manifest records 31 production call sites with conformance tests.
See [release verification](#release-verification) for deployment evidence and the
boundary between controlled lifecycle tests and live external delivery.

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

Tool paths are capped at 32 components below their scope root and at 4096 bytes
as an absolute path (Linux `PATH_MAX`). The caps apply to Read, Write and Edit and
to every Glob/Grep entry. A deeper Write is refused before any directory is
created. Searches skip deeper entries and walk with an explicit stack, so an
existing deep tree cannot exhaust the interpreter stack (#1042).

No client or model input can end the bridge process short of SIGKILL. Each
request line passes through one `safe_dispatch` wrapper. Unparsable input
(invalid JSON, invalid UTF-8 or excessive nesting) gets JSON-RPC `-32700`. A
value that is not a request object, or has an unusable id, gets `-32600`. Both
use a null id because no id can be trusted. A request with a readable id but a
wrong `jsonrpc`/`method` gets `-32600` with that id. A `tools/call` whose params
are not an object, whose `name` is not a string, or whose `arguments` are not an
object gets `-32602`. Any other unexpected failure gets `-32603` with no details.
Tool-level `RecursionError`/`MemoryError` become ordinary tool errors. Lines
longer than 64 MiB are discarded without being buffered. They get `-32600`,
carrying the id only when it is the compact request's leading field. Notifications
and client responses are never answered. We checked the null-id replies against
Codex's MCP client (rmcp 3.2.0 in codex-cli 0.154.0). It parses an error without
an id as `JsonRpcError { id: None }`, logs it and drops it, and it never replies
to an error. A null-id reply therefore cannot complete or stall a pending call,
and cannot start an echo loop.

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
public dependencies can be retrieved through the read-only gateway described below. UTF-8 source changes pass through
the original write guards and concurrent-edit checks before being copied back.
Binary source changes and file deletions also reconcile under the source-build
profile, retaining scope and concurrent-edit checks; build artifact directories
remain excluded. A profile with a matching text-only Write hook rejects binary
or deletion reconciliation explicitly, because those effects cannot faithfully
be represented as a text Write event. Real synthetic Cargo and npm tests verify
execution, private dependency installation/reuse, and scoped source reconciliation.
The actual project installation, build and Node suite also pass through the VM.
The HOME identity is preserved as an OS environment value without granting read
access to files in that home.
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
QEMU device, share and sandbox options follow its
[invocation reference](https://www.qemu.org/docs/master/system/qemu-manpage.html).

The runner `scripts/codex-build-vm.py` executes a disposable snapshot in
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
root and nested npm workspace dependencies mount read-only. Source reconciliation runs only after VM
shutdown and retains original Write hooks and concurrent-edit checks. A real
bridge test verifies npm dependency loading, loopback, process sessions, source
updates and guard denial. Live Codex QA ran a Cargo socket test through the
packaged bridge, verified its successful tool audit and confirmed build outputs
stayed out of the source worktree. A durable private runtime was provisioned with
a package/version/hash record; a second live test passed using default discovery
without an environment override. This did not restart or deploy the daemon. Cache reuse and public dependency
retrieval are covered by the verification below; runtime limits are documented
in BUILD-VM.md. A real VM test runs npm in a nested workspace whose path contains spaces,
loads its local dependency, verifies dependency writes are denied and reconciles
the generated source output. Real-VM Python tests
require `JARVIS_TEST_VM_CONFIG` pointing to owner-private runtime configuration;
the live adapter test uses default discovery or the daemon override.

## Fallback diagnostics

Required MCP startup failures use fixed readiness categories for initialization,
timeout and missing tools, rather than an unsupported-method error or a traceback
containing configured paths. The Codex adapter maps these categories to local
readiness failures before logging or returning native CLI details, so they do not
create provider-outage cooldowns. Synthetic tests verify that private configuration
markers do not appear in the returned diagnostics.

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
in the error chain for existing cooldown/retry callers. Doctor reports constructor-time capability exclusions and binary/auth preflight;
concrete sandbox and MCP readiness are validated when a request starts.

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
The memory server's four explicit read contracts (search, recent, conversation
search and thread read) also remain fresh while a write is uncertain. Memory
writes and unknown tools still require reconciliation before they can run.
An uncertain write returns a safe, actionable tool error without its arguments.
Tests cover restart, ambiguous connection failure, corrupt and
symlink state, model-scope exclusion and actual bridge receipt reuse.

Production dispatch now assigns private journal paths for write/agentic calls and
forwards recorded progress to the next provider. Requests with a channel turn id
have a stable hashed identity across restart, independent of refreshed clocks,
owner context and provider settings. WhatsApp uses the stable message id as well
as the chat id, so separate turns do not share receipts. Scheduled loops pass a
persisted occurrence identity derived from the loop id and last recorded run;
a crash before that record reuses the same journal. Callers without an id (or
with the empty audit placeholder) get distinct journals and still need
caller-owned restart identity if they resume work. Claude receives pre-tool,
post-tool and failed-tool hooks using the documented [hook event contract](https://code.claude.com/docs/en/hooks).
The pre-tool hook persists started state; successful post-tool events normalize
results for Codex. The generated command turns script startup failures into exit
2. Tests execute that command, including quoted paths and blocking results, and
exercise receipt forwarding through the dispatcher.

Primary hooks now also block a new tool-call id from repeating a completed
MCP or broker service action in the same request. Both providers compare parsed
command arguments, ignoring shell quoting, whitespace, descriptions and timeout
changes; they retain the original inputs in the journal. Completed local builds
can run again after source edits. Separate request journals permit a new user
request with identical arguments. Intentionally repeating the same external
action inside one request still needs explicit operation identity support.

A live two-provider fixture passed: Claude invoked a synthetic MCP counter,
its real hooks persisted the completed receipt, and Codex then requested the
same action without being given recovery prose. Codex returned the first
receipt and the counter remained one. This verifies actual primary hook and
fallback broker interoperability for a completed action.

The disconnect variant also passed with both real CLIs: the fixture performed
its effect and exited before sending a response. The primary journal retained
`started`, Codex received an audited reconciliation refusal, the journal stayed
unchanged and the effect counter remained one. This verifies safe refusal after
an ambiguous transport failure. The extended fixture then checks the synthetic
service counter, records an operator completion receipt through the recovery CLI,
and resumes Codex. The response contains the verified receipt and the counter
remains one; no second external effect occurs.

Uncertain effects can now be resolved with an owner-only recovery command after
checking the authoritative service. Decisions require the exact operation
fingerprint and evidence; they preserve the original attempt and a timestamped
receipt. Verified completion reuses the observed result without another effect.
Verified absence permits one fresh attempt, which receives normal journal and
approval enforcement. Active requests, unverified cleanup, stale fingerprints
and conflicting decisions are refused. The model cannot invoke this recovery API.
See [operator recovery](#operator-recovery-for-uncertain-effects) below.

Argument matching does not identify all semantically duplicate actions expressed
through different commands or tools. To intentionally repeat an identical external
action, use a new user request so it has a separate operation journal. Journals are
retained privately. The daemon removes only finished journals idle past a grace
period; unresolved receipts are never removed, by the sweep or by hand, to force
progress (see [journal retention](#handoff-journal-retention)). Hook observations do not substitute for verified
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
process and verify detached work stops before recovery. Production query and loop callers supply stable turn identities; markers are
never cleared by age.

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
substitute for MCP initialization or model output contracts. Provider execution/output coverage is recorded separately in the manifest. All
31 entries now have named conformance evidence; inventory and policy checks alone
do not prove operational rollout.

Each entry also records its `model_tier` (`quality`, `fast`, or `preserved` for
wrappers that keep the model of the options they receive) and a
`tier_rationale` (#1046). The same AST scan fails on any production
construction that leaves `model: None`: no `--model` flag means the spawned CLI
inherits the owner's interactive model (#448). It also fails when a tier spelled
at a `ReasonerOpts::pinned(ModelTier::…)` call, or implied by a literal model id,
disagrees with the manifest. Core presets whose model comes from a helper are
checked at runtime against `providers::tier_of`.

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
overrides.

The controlled `live_fallback_pipeline_resumes_after_independent_reviewer_recovers`
test now runs the production auto-ship state machine with real providers and Cargo
checks, an isolated local Git remote, and a simulated GitHub transport. A latched
primary routes scoping/building to Codex. Missing independent review capacity
preserves the draft, including on resume, without further mutation calls. After
synthetic primary recovery, independent review gates the merge; a fresh checkout
of the exact merged revision passes additional acceptance tests. Both paths clean
the pipeline worktree. This does not prove public GitHub or daemon deployment QA.

Review revisions, conflict repairs, and privacy repairs stay within the draft's
recorded builder providers. A recovered primary remains available for independent
review instead of becoming another author. Unknown provenance or unavailable
builders fail closed; missing review capacity does not consume rejection rounds.
Behavior tests cover provider recovery and builder failure without dispatching an
independent provider as a replacement author.

The completed provider conformance suite includes:

| Production boundary | Current evidence |
|---|---|
| Classic drafting | Both providers draft with and without scoped wiki context |
| Seven communication channels | Both providers generate programs consumed by the real Deno runner and dispatcher; each persists a pending draft with the original generated source |
| Optional social MCP wrappers | Both providers perform authenticated reads through both production constructors; deterministic negative probes enforce the original read-only guard |
| Signature, voice, journal and social adaptation | Both providers pass the actual extraction/composition consumers; social adaptation must complete provider calls instead of silently returning the source |
| Scheduled Discord/Slack digests | Real scheduler ticks with synthetic stores and a capturing broker; Claude quota refusal invokes Codex, then skips the latched primary; successful delivery is throttled |
| Query and document delivery | Real query handler, original-byte attachment preparation and latched-primary exclusion |
| Auto-ship | Controlled full lifecycle with real providers/builds, local Git and simulated GitHub; independent review after primary recovery, merge and fresh-checkout acceptance |
| Selftest | Actual candidate CLI returns PONG with a healthy primary and an isolated pre-latched primary; deterministic binary tests cover routing and healthy-primary preference |

Live channel fixtures have no external sending integration. These checks establish
provider and consumer contracts, not live account delivery or deployed daemon QA.

Output parity includes model tier selection, code-mode parsing, images, last
assistant block versus complete transcript, original attachment markers, tool
audit records, cancellation and shared CLI-gate lifecycle. A text-only selftest
cannot stand in for any tool-using profile.

## Current verification

- The production-shaped wiki-ask quota regression now passes, alongside routing
  tests for all four capability classes and live handler/delivery fallback QA.
- Bridge tests cover file read/write/edit, nested writes, bounded search, tool
  declaration, traversal and intermediate symlink escapes, sensitive paths,
  command parsing, and guard denial/crash/malformed-response handling.
- `BridgeResilienceTests` pin the path depth/length caps for reads and writes,
  Glob/Grep over a 1500-level tree, and a stdio bridge that keeps serving after
  deep paths, malformed lines, non-object requests and invalid `tools/call`
  params, with the JSON-RPC codes above.
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
  downloaded formats use original-byte attachment delivery, without conversion.
  The query handler and attachment-preparation path pass live provider QA;
  verification does not send synthetic files to live external channels.
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
  Live independent Claude review after Codex builds and the controlled full
  lifecycle have passed. Deployment is verified in the release receipt below.

## Integration contracts and release gates

Both optional social drafting presets now exercise their production policy
through an authenticated local HTTP MCP fixture. Reads reach the endpoint with
the configured bearer header; the original read-only hook rejects an offered
write tool before any remote call. HTTP initialization and tool-call timeouts
produce the sanitized `mcp_timeout` readiness category without retrying the
request. A timed-out mutation retains its uncertain journal entry. These are
synthetic transport and policy contracts, not live social-account operations.

Codex's production source-inspection presets have live scope/review coverage:
a synthetic arithmetic defect yields a parseable implementation plan and
acceptance criteria, and a constant-return patch is rejected after reading
source and inspecting its Git diff. Source bytes remain unchanged by both
passes. A repeatable broker test also verifies that these presets reject
Write, Edit, Git commits and build commands. This covers the inspection stages. The controlled lifecycle described above
also covers merge and fresh-checkout acceptance; deployed CLI QA is recorded below.

The shared live output-contract suite passed through both Claude and Codex:
interval parsing, missing-timezone errors, archetype selection, newsletter
triage and insufficient-sample tone descriptors. It also generated and executed
a synthetic draft through the real Deno code-mode runner, with exactly one
draft operation and no sending capability. The loop prompt now explicitly
requires clarification inside JSON after the Claude baseline returned a prose
question that its consumer could not parse. These checks cover shared output
contracts; they do not establish every channel's integration behavior.

Native web calls now appear in the common audit log with the Codex provider,
query and native action. A live public-page fixture passed through the adapter
and verified the audit entry. The CLI uses the same `web_search` event for
searches and page opens, sometimes with an opaque `other` action; it does not
include page content in that event. Records preserve this limited evidence.
Because native web combines search and retrieval and bypasses bridge hooks,
the adapter requires both WebSearch and WebFetch and rejects matching web
hooks. The production query preset's file-only hooks remain supported.
Single-web-tool and web-hook profiles need a guarded implementation before
they can be accepted; they are not silently broadened or claimed as parity.

The implementation passed independent review, CI and merged release rollout.
Candidate contract receipts remain distinct from the deployed CLI and running
binary verification recorded below.

All fixtures and publishable receipts must use synthetic data. Live account
configuration, private correspondence and raw runtime logs stay outside this repo.


### Fresh Node worktree verification

The bridge can reuse matching installed dependencies from a linked worktree's
registered main checkout. It checks lockfiles and dependency declarations before
mounting selected package roots read-only; generated lockfiles and unrelated
installs outside the requested project are excluded. Changed resolution inputs
are rejected rather than tested against a different dependency tree. Synthetic
contracts cover matching resolution, script-only edits, mismatches, symlinked
manifests, unrelated installs, and a real guest build from a fresh Git worktree.
A separate fresh checkout of this project passed `npm run build --offline` and
all 26 `npm test --offline` tests through the VM bridge, then was removed.
Uncached public packages can now be fetched through the guest dependency gateway.


### Shared output-contract verification

The six live `provider_output_contracts` tests pass through both Claude and Codex.
They cover interval parsing and missing-timezone handling, archetype selection,
newsletter triage, insufficient-sample tone output, and a generated draft executed
by the real code-mode runner with a non-sending dispatcher. Additional fixtures
verify exhaustive digest coverage, booking-link extraction through the production
ask detector, and read-only linting with audited source inspection and a reported
broken link. The lint preset explicitly identifies the configured wiki root so
schema examples do not imply an extra `wiki/` subdirectory. These receipts verify
the named contracts; the inventory table records the other profiles. They do
not establish deployment.


### Shared wiki mutation and migration contracts

Both providers pass the synthetic resume and ingest contracts. Resume seeding
preserves existing non-resume facts, adds sourced skills without inventing contact
information, and emits the required `wrote:` marker. Ingest preserves prior facts,
records a cited preference, updates the log, and leaves the derived index unchanged.
The ingest preset explicitly identifies its root and requires a final completion
acknowledgement: an earlier Codex run performed the writes but returned no response,
which the adapter correctly rejected rather than silently reporting success.

The CLI's shared live migration tests use the production prompt, YAML parser,
citation filter and patch application. Both providers produce supported cited
fields, preserve original frontmatter/body content, leave source files untouched,
and return an empty patch for a page without evidence. The page request explicitly
names migration as the task so thin pages do not trigger a clarification response.


### Completed channel and integration conformance

The latest full workspace regression passed 2,479 tests with zero failures across
72 targets; 38 live/environment-dependent tests remain opt-in in that command.
The newly added paired live suites were run explicitly: optional HTTP MCP,
seven-channel code-mode drafting, classic drafts with optional wiki context,
signature/voice/journal/social formats, and scheduled digests. The current query
fallback and controlled auto-ship lifecycle also passed again. The capability
inventory now rejects entries without a named conformance test (red regression
confirmed, then all five inventory tests passed).

These receipts apply to the candidate worktree. They do not prove deployment.
Recovery and public dependency provisioning have separate verified contracts
below. Final review, CI and deployed revision/CLI verification are recorded in
the release receipt.


### Operator recovery for uncertain effects

An error or timeout does not prove that an external write failed. First inspect
the authoritative service using read-only access. Stop the affected request and
let normal supervisor cleanup complete. From the trusted checkout, inspect its
owner-private journal (the directory is under
`~/.local/state/augmentagent/reasoner-handoffs/`):

```sh
python3 scripts/codex-tool-bridge.py --handoff-status /absolute/private/request/operations.json
```

Status returns indexes, tool names, states and fingerprints; it omits arguments,
results and evidence. Do not publish the private journal. Create a private JSON
decision file with the returned `index` and `fingerprint`, an `outcome` of
`completed` or `not_applied`, and nonempty `evidence` describing the authoritative
check. `completed` also requires the observed successful MCP-shaped `result`, for
example `{"content":[{"type":"text","text":"synthetic-created"}]}`. Only choose
`not_applied` when absence of the effect is established, not merely because a
lookup is inconclusive. Submit it through stdin:

```sh
python3 scripts/codex-tool-bridge.py --handoff-reconcile /absolute/private/request/operations.json < /absolute/private/decision.json
```

The command locks against both journal execution and provider startup, refuses
active/unverified requests and stale decisions, and fsyncs the decision before
reporting success. It never clears cleanup markers or exposes journal payloads
in errors. A completed decision supplies the result on replay; an absence decision
keeps the original attempt and permits a new one. Resume the same logical request
through its normal entry point. Keep receipts through rollout and rollback;
older adapters reject unknown receipt states rather than replaying them.
The retention sweep below never removes a journal with a `started` row, so an
uncertain journal waits for this command however old it is (as long as no
cleanup marker is left over; see below).


### Handoff journal retention

Each write or agentic dispatch creates a request directory under
`~/.local/state/augmentagent/reasoner-handoffs/`, and its journal keeps the full
tool arguments and results. Without retention that directory grew by about a
gigabyte a day (#1035). The daemon removes a request directory only when all of
these hold:

- **Idle.** No `operations.active` lifecycle marker exists. This is the same
  predicate the resume gate uses; a marker whose cleanup receipt would verify
  still counts as active.
- **Settled.** Every journal row is `completed` with its result, or
  `not_applied` with operator evidence (or no journal was ever written).
  A `started` row (an uncertain outcome), any other or future status, and an
  unreadable, oversized, linked or non-private journal all keep the directory.
- **Expired.** Nothing in the directory changed for the grace period:
  `AUGMENTAGENT_HANDOFF_RETENTION_HOURS`, in whole hours, **default 24**.
  Values above 8760, or below the floor, fall back to the default with a
  warning. The floor is the longest CLI-gate wait plus one hour, rounded up
  to whole hours: 3 hours at the default `AUGMENTAGENT_REASONER_TIMEOUT_SECS`
  (write and agentic calls may queue for twice that timeout before their
  provider starts, and the request has no lifecycle marker while it queues).
  A larger timeout raises the floor, and the default with it if needed.
  Dispatch refreshes the directory timestamp under the lifecycle lock every
  time it addresses a request, so a turn that keeps retrying keeps its receipts.
- **Confirmed.** The daemon's previous pass already saw the request settled,
  expired and unchanged. "Unchanged" compares metadata, not contents: the
  device, inode, nanosecond modification time and length of the directory and
  of every entry. A restarted daemon removes nothing during its first
  interval, which gives turns replayed at startup time to re-address their
  journals however long the daemon was down.

The sweep runs at daemon start and then hourly, on the blocking pool, and logs
one `handoff journal sweep` INFO line with `removed` and `kept` counts (split
into recent, active, unfinished, pending, busy and untrusted). It takes the
lifecycle and journal locks without waiting and skips a request whose lock is
held, re-checks the request under both locks before removing it, removes
entries relative to an opened directory without following links, and refuses a
root or request directory that is not owner-private or holds unexpected
entries. Failures are logged and never stop the daemon.

What it never touches, and for how long:

- **Uncertain journals** (a `started` row, or any status it does not recognise)
  stay until the recovery command above records a decision. The next passes
  then treat them like any other finished journal.
- **Journals with an `operations.active` marker are retained indefinitely.**
  A marker means a provider is running or its descendants' cleanup has not
  been verified. The recovery command refuses while a marker exists and never
  clears one, and the sweep never clears one either. The only path that clears
  a marker today is the resume gate, when the same turn is dispatched again and
  its cleanup receipt still verifies. The daemon has no SIGTERM handler, so a
  service stop or restart during a call leaves a marker behind. Most such
  turns are never dispatched again (calls without a turn id get a fresh
  request), and the receipt lives in a temporary directory that may be gone.
  Those journals stay on disk until a marker-clearing mechanism exists,
  tracked in #1071.

Do not delete handoff state by hand.

An on-demand pass is available. `--dry-run` takes no locks, changes nothing and
prints the effective grace and where it came from (this shell's environment or
`.env`, or the default; the daemon reads its own environment, which may differ):

```sh
augmentagent handoff-prune --dry-run
augmentagent handoff-prune --yes
```

A removing pass requires `--yes`. Unlike the daemon, it does not wait for a
confirming pass, so run it only with the daemon stopped for a reason, and not
during an outage in which a loop occurrence was interrupted (its replay after
restart would find no receipts). It refuses while `augmentagent.service` is
active, or when `systemctl --user is-active` cannot tell, unless `--force` is
given.

`augmentagent doctor` reports (read-only) the number and size of request
directories, finished journals past grace, and, for information, uncertain
journals and lifecycle markers. It warns above 5000 request directories or
3 GiB, and when any finished journal has been past grace for more than two
sweep intervals (plus 15 minutes). A live sweep removes such a journal within two
intervals, so this catches a dead sweep within hours.

Limit: a turn first re-dispatched more than the grace period after it last ran,
and more than one sweep interval after the daemon started, is treated as a new
request with an empty journal.


The recovery change passed all 73 bridge tests, including both provider hook
paths, real CLI status/decision submission, concurrent lifecycle locking, stale
and malformed decisions, private-error output, and late-result rejection. The
Rust handoff regressions passed, and the real two-provider disconnect/reconcile/
resume fixture passed with one observed effect throughout.


### Writable dependency installation

A new real-VM regression reproduced `npm ci` failing with EROFS because existing
installed dependencies were mounted read-only. Installation commands now use a
private writable copy; npm and its lifecycle scripts execute inside the guest.
The subsequent build consumes the installed result read-only. The synthetic
local-tarball fixture verifies the new package, its install-script output, a
passing build, untouched host dependencies, invalidation after manifest changes,
and removal when the bridge closes. The real-VM checks also cover cache cleanup and a concurrent owner manifest edit.
The latter first reproduced a stale lockfile reaching the checkout before denial;
the pre-sync manifest check now rejects it without writing that lockfile. The
full bridge suite passed all 76 tests after these changes.

This closes writable local installation and reuse within one bridge session.
The public dependency gateway below supplies uncached npm and Cargo packages.
Final release review and deployed CLI QA have separate receipts below.


### Uncached dependency verification

The networkless guest now has a read-only public registry gateway. Canonical
HTTPS URLs are preserved through a guest-only CA; fixed host origins, GET/HEAD
restrictions, credential isolation, request/byte limits and the enclosing build
deadline remain enforced. See [runtime setup](BUILD-VM.md#public-dependency-gateway).

Live VM fixtures fetched a public npm package into an empty environment, compiled
an uncached Cargo dependency, then rebuilt it offline using the private session
cache. Extracted crate sources and guest-modified Git checkouts are not retained
between builds; an offline rebuild rejects a corrupted cached archive. They
verify canonical lockfiles, unchanged operator caches, source-cache
exclusion, blocked direct networking/HTTP writes and an inaccessible signing key.
The local install fixture now compiles and loads a synthetic native Node addon.

A fresh worktree of the real project passed `npm ci --no-audit --no-fund`,
`npm run build --offline` and all 26 `npm test --offline` tests. The native SQLite
addon compiled against matching system Node headers in the guest. That worktree
was removed afterward. Runtime setup now pins a private compiler copy without
changing the operator's existing Rust installation. These checks establish build
and dependency contracts; they do not claim service deployment or a merged PR.


## Release verification

PR #1021 merged at `92dcaa9` after required CI passed. Independent reviews of
execution, handoff/reconciliation, writable installations and the public registry
gateway found no remaining blocking defects after corrections. The final cache
review confirmed that extracted crate sources are not retained, Git dependencies
come from the provisioned cache, and explicit offline behavior is preserved.

The optimized CLI and memory server were built from the exact merged source tree.
The installed CLI and the running daemon executable have the same SHA-256 as the
tested release candidate; the deployment receipt and previous binaries remain
private. The daemon restarted successfully and reported zero automatic restarts.
Existing cooldowns and handoff journals were preserved; automatic updates were
re-enabled after recording the verified build revision.

Post-deployment CLI QA used an isolated synthetic wiki/database and a private
Claude cooldown. Real Codex completed Glob/Grep/Read/Write/Edit, memory_recent,
and the allowlisted local `augmentagent loop list --json` command. Exact edited
bytes, original document bytes, the delivery marker and provider-attributed tool
audit all passed. A first candidate probe failed its exact text-byte assertion;
a second probe with an explicit final-newline requirement passed, as did the
installed-binary probe. This is an execution receipt, not a guarantee that model
outputs never need validation.

The current query-handler live test passed with two requests: original attachment
bytes survived delivery preparation, and the second request skipped the latched
primary. The controlled auto-ship lifecycle uses real providers, Cargo and Git
with simulated GitHub operations; it verifies independent review, merge and
fresh-checkout acceptance. These checks do not send test messages or attachments
to live external channels or create public test PRs.

Final regression evidence: 2,479 Rust tests passed with no failures across 72
targets (38 opt-in tests run separately where applicable); 79 bridge, 8 VM,
5 registry gateway and 5 helper-packaging checks passed. A separate live test
rejected a corrupted cached crate on an offline rebuild. A clean project install,
build and all 26 Node tests passed inside the VM. Tracked-file privacy, release-tree
secret and branch-history secret scans passed. Doctor reports Codex as available
for every declared capability class; concrete guard/MCP readiness still runs per
invocation.
