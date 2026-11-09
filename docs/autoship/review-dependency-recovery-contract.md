# Coherent immutable review dependency recovery — #1952

Status: scoped C1 design ready for test-first implementation after fresh-reader,
source and all32 mapped seam checks. C2 is not design-ready. This is not code
approval, rollout or activation authority. Owner #1709; parent #1703.
Incident identity: [PR1944 receipt](1703-pr1944-review-dependency-proof.json).

## Problem and outcome

At candidate3c00c759, self_improve.rs is 967,956 bytes. The serialized native
request limit including system text is 524,288 bytes. Initial review shards
large source, but missing_paths recovery requires a whole file to fit one
request. Merely combining independent approvals of more byte shards does not
prove anyone evaluated the missing relationship across them. That multipart
proposal was rejected after fresh-reader audit; it must not be implemented.

The replacement lets the same reviewer role request precise immutable source
items/ranges and then receive all requested dependencies together with its
original assigned evidence in one bounded recovery request. Approval is still
the actual independent review decision. If necessary coherent context cannot
fit or remains insufficient, retain an explicit incomplete/budget hold. Never
turn byte coverage, an index, selected source or CI into a review verdict.

## Decisions and protocol

D1. Preserve exact head/base/contract/evidence and native independence. Keep
MAX_PROMPT_BYTES (512 KiB), MAX_RESPONSE_BYTES (128 KiB), three-role coverage,
all initial candidate/caller byte coverage, prior findings, final adjudication
and current admission/storage authorities. No extra selector models, new
scheduler, native continuation, per-part state or approval authority.

D2. Add a strict source-request response variant for incomplete decisions:
missing_sources is an array of at most16 objects. Each contains a literal path (nonempty, at most4096 UTF-8 bytes),
criterion_ids (nonempty unique known registered criteria), a nonempty question
(max4096 UTF-8 bytes), and one selector:
- whole_file;
- rust_item with a literal Rust identifier name;
- bytes with start_byte inclusive and end_byte exclusive.
Represent selectors as a deny_unknown_fields tagged enum. Names are Rust
identifiers accepted by the existing syn dependency, with optional r# prefix;
there is no regex/glob/path expression. Validate the request name using
syn::parse_str::<syn::Ident>, then compare with parsed declaration identifiers
after stripping one leading r# from each. Exact case matters; foo and r#foo
match each other in both directions. Keyword identifiers require valid raw
spelling to pass the request parser. Preserve original spelling in source
evidence; normalization is lookup identity only. Byte bounds are
checked usize values with start < end, strict UTF-8 boundaries and end <= blob
size. Requests are data, never tools or host filesystem permission.

Keep legacy missing_paths as whole-file requests with implicit all-current-
criteria question. Reject simultaneous missing_paths and missing_sources,
wrong-field types, unknown selectors/fields, excessive requests and source
requests on approve/changes_requested. Preserve the old public parser wrapper
for path-only callers/tests; a new production parser returns typed requests.
A path-only wrapper must reject a typed range/item request, not discard it.

Canonical adjacent nesting is fixed, for example:
```json
{"missing_sources":[{"path":"src/logic.rs","criterion_ids":["C1"],"question":"Does caller use the callee result correctly?","selector":{"kind":"rust_item","name":"caller"}}]}
```
The other selector objects are exactly {"kind":"whole_file"} and
{"kind":"bytes","start_byte":0,"end_byte":128}. No selector fields live
beside path/question/criterion_ids, and fields belonging to another kind
are rejected. This is nested, internally tagged selector JSON; all examples
are inside an otherwise valid incomplete response with binding/findings.

D3. Render proven candidate regular-file chunks and direct caller-stream
chunks with exact UTF-8 byte offsets and immutable path/revision/OID/size
metadata. Replace candidate #part numbering for those raw bytes. Preserve
source bodies and full role coverage. Metadata is not source coverage.

Do not label rendered text as raw file coordinates. candidate_snapshot
currently prepends a deletion explanation to base content, and inline caller
context is serialized JSON containing files. Preserve these existing bodies
as explicitly rendered historical/context evidence with offsets into that
rendered body, its body hash and bound base/head identities. They are not
addressable candidate source ranges. Initial deletion evidence may retain
the already-bound base bytes; recovery continues to resolve candidate head
only. A missing deleted path is unavailable, not permission to read another
revision. Symlink blob text may remain initial diff evidence but must not be
mislabelled a regular-file source or grant recovery permission.

Instructions ask for concrete item names or byte ranges when a whole
dependency cannot fit. No function index is required: names can come from
observed declarations/call sites, and exact byte windows refer to raw source
offsets. An unknown name is not guessed or replaced with an approximate match.

D4. Initial request packing uses at most MAX_PROMPT_BYTES/2 including system,
contract, issue, prior findings and all other actual serialized fields. This
leaves recovery room without truncating an assigned group. The small complete
candidate fast path remains when it fits that initial bound. Larger candidates
are losslessly packed into more partial groups, with every group required for
every role. Diagnostic and production planners use this same packing/count.
The recovery request uses the unchanged full MAX_PROMPT_BYTES. All original
assigned files/context and every requested dependency/question must fit
together; do not drop original evidence or choose a fitting subset.

## Immutable source production

D5. Reuse the existing no-replace-objects Git reader, literal tree lookup and
credential/control-path policy. Resolve each path to exactly one regular tracked
blob at the bound head; reject symlink, submodule, directory, missing/denied
path, binary and invalid UTF-8. Also retain requested_evidence's stronger
control-character and credential-directory exclusions. Never read working-tree
files, runtime .env, another revision or a model-supplied blob identity.

whole_file retains the existing bounded full-file behavior. For rust_item,
check immutable size before reading; at most8 MiB of source may be parsed,
matching the existing bounded inline caller working set. This is an item-
lookup working-set cap, not permission to stage 8 MiB into a model request.
Reuse syn::parse_file and Visit already used by self_improve_review_symbols.rs.
Collect all matching fn/struct/enum/union/trait/trait-alias/type/const/static/
inline-or-external-mod items, named macro declarations, impl fn/const/type,
trait fn/const/type and foreign fn/static/type. Field/variant names, pattern
bindings and macro invocations are not item declarations; request their
containing item or exact bytes. Do not infer a semantic call graph or expand
macros. Use whole lines spanning each exact AST item, so non-ASCII column
units cannot shift byte selection. Deduplicate identical/overlapping ranges.
Include enclosing impl/trait/foreign-block headers and enclosing inline
module attributes/header as companion ranges for selected members; include
their attributes and where clauses. Their source is required context.
Label each range by literal path and absolute byte interval. A duplicate name
supplies all matches together or refuses budget; never silently choose one.
No match, parse failure or source above the item working set is typed
unavailable evidence, not an empty successful lookup. Precise byte requests
remain available for large/non-Rust/ambiguous sources.

For bytes, reuse caller_stream's strict UTF-8/complete-EOF producer. Drain the
original immutable stream, collect only requested bounded intervals, verify
original blob size and child success, then publish those exact bytes. The
stream's 8 MiB/256-entry batches are working-set bounds, not a whole-file cap.
Refuse a requested raw range above MAX_PROMPT_BYTES before allocation. Verify
HEAD still equals binding before recovery publication and before the call.
Only fully validated reads can become supplied evidence.

## Recovery, persistence and failure semantics

D6. Keep the current maximum two reviewer observations per original group.
A valid first incomplete response may produce one coherent recovery call.
A structured-format correction consumes the second observation and cannot
then start another recovery call. The second response must complete the
assigned group or leave it incomplete. If any requested dependency is denied,
unknown, unsupported, malformed, outside its ranges or otherwise unavailable,
abort before observation two; do not pay for a known-incomplete subset.
Return through existing run_impl error/unavailable_from_error handling, with
available=false and no completed group, cache approval or merge authority.
Size/working-set refusal uses existing typed EvidenceBudget; other producer
failures retain their contextual errors. C1 does not pretend that existing
generic outer failure projection or failed-finding persistence is repaired;
those are explicit C2 exits. The current in-attempt GroupReview retains valid
findings while constructing a successful response; durable failure retention
remains C2. Partial source is not published as successful recovery. No partial approval union or third
call/generation. Selector extraction uses no model calls, so worst_case_calls
remains 6*initial_groups+2, calculated with D4's actual initial groups. Native
quota/resource charging and immutable canary reservation remain unchanged.
Diagnostic runs still require fresh actual observations and do not reuse
normal fragment successes/failures as diagnostic proof.

D7. Preserve the original incomplete response's valid findings and every
valid recovery finding using existing GroupReview retention. Cache completion
only after actual independent native responses and current binding checks.
Store requested exact source ranges under the existing recovered-source
namespace, include them in finding_evidence, and forward actual bytes through
existing final_source_evidence/final-authority selection. Neither source
hashes nor questions are findings or verdicts. Question criteria guide
evidence selection/review only and cannot remove registered requirements.

No growing parent/part graph is introduced. A normal completed record has
at most two requests/responses. Prove a conservative encoded bound of
4*MAX_PROMPT_BYTES + 12*MAX_RESPONSE_BYTES +64 KiB, below the existing8 MiB
fragment-writer limit, from source-map duplication, normalized findings and
bounded validated actor/binding/session metadata. Check that bound at the
production serialization boundary. Before any reviewer call, require the
encoded static binding/actor/model/role/original-hash metadata (plus two
64-byte request hashes) to fit32 KiB. Actual returned native IDs must be
nonempty ASCII graphic strings of at most512 bytes; retain exact IDs, never
truncate or derive replacements. Native VM observation already caps them
at256 bytes; independently verify this production path in source/tests.
Oversized/invalid actual identities are transport/protocol failure, not a
valid completed observation or deterministic source failure. These checks
leave the conservative64 KiB metadata remainder; no unbounded response spool.

## Staged delivery, without closing the parent early

C1 is the D1–D7 coherent-source protocol/producer/packing component. It does
not introduce multipart recovery, a growing parent graph, terminal cache
records, new admission counters or a lost-native-result replay operation.
Current completed-fragment, exception, custody and per-workflow actual-charge
paths remain in force. Within a successful recovery, all initial findings are
retained and forwarded to final authority. C1 must not claim to solve failed-
review history retention, unchanged paid retries or generic failure projection.

C2 remains required in #1952/#1709/#1706: preserve partial observations/source
and findings through failed recovery, persist a deterministic unsupported-input
hold without repeated unchanged model spend, distinguish terminal source/
representation failures from transient admission/transport/storage failures,
and implement source/contract/epoch/model/policy wake semantics. This needs
its own reviewed lifecycle contract before implementation. Current native
outcome uncertainty, lost results and cache-publish failure cannot be promoted
into successful observations, resource release or retry authority. Diagnostic
runs must retain fresh actual observations and immutable reservations.

Source inspection identifies the existing integration points for C2:
IndependentReview can remain available=false with structured partial evidence;
recorded_verdict then returns None. Issue-backed resume already records
structured receipts before handling unavailable. Source-less adoption sets its in-memory previous value and records a
publication verdict only when recorded_verdict is Some; its subsequent
episode::evaluate also records any supplied structured evidence under
episode-evidence-* even if available=false. Reuse that existing hook rather
than inventing a duplicate store. It does not prove the failed-recovery
producer currently returns structured partial evidence, or establish its
restoration/wake lifecycle.
Review-epoch invalidation deletes current indexes/fragments but preserves
workflow_event history. current_review_history intentionally only authorizes
the current binding/epoch. verify_fresh_review_origin also intentionally
requires the current binding; do not weaken either to recover historical
facts. A historical-finding reader/resolution transition must have a separate
non-approval contract and cannot validate an old origin as current authority.

Fresh-reader findings about whole-file multipart reasoning are resolved by
D4–D6: the actual reviewer receives original evidence plus all requested
related source in one request and may still refuse completeness. The former
new parent/part graph and per-part lifetime counter are cut from C1. Encoded
success-record growth is handled by D7 without raising the writer limit.
Historical failed observations, wake/counter identities and lost-result
producers are explicitly C2 gates, not silently treated as implemented.

C1 may be handed to implementation only after source/fresh-reader/all mapped
C1 seams are checked. That scoped handoff does not close C2, #1952, #1709 or
#1703. C1 code must be reviewed, normally rolled out and exercised on the
preserved PR1944 command; if the actual required coherent source cannot fit,
this is still an unresolved incident, not a successful component live exit.

## Test-first delivery and readiness gates

For C1, first verify the source/API assumptions, fresh-reader findings and
every mapped C1 seam. Then use a scripted provider boundary through the real production
review/recovery adapter and a real immutable disposable Git source >512 KiB.
The first response requests two distant related Rust items; recovery must
supply both together with original evidence. A seeded cross-item defect must
produce changes_requested, not composed local approvals. Record the current
actual budget/unsupported-protocol failure as RED before implementation.

GREEN must cover full candidate/caller initial coverage, exact requests and
source/companions, duplicate names, raw identifiers, macro/impl/module context,
UTF-8/newline/byte offsets, hostile/unknown fields, source-path exclusions,
unsupported Git entries, absent/malformed source, exact/+1 initial/recovery/
response/cache budgets, escaping, original findings, format correction,
incomplete second response and existing completed-cache/crash/binding/native
identity behavior. Failed-review history, free deterministic hold and wake
acceptance remain mandatory C2 exits. Verify diagnostic planner/reservation and
final-authority actual-source consumption. The v1 PR1944 incident records only a whole-file missing path, not the
underlying item pair/question. Do not invent one and credit it as incident
acceptance. Before implementation, a real public-source profile may measure
item lookup/serialized sizes on immutable3c00c759, explicitly labelled
construction evidence. The required incident-resolution probe is parked
until the first preserved normal PR1944 attempt under the new protocol
supplies its actual typed request, criterion/question, original assigned
group and exact serialized metadata. Owner #1709/#1952 must capture and
replay that input, then establish actual independent completion. This
deferral does not block generic C1 TDD/handoff, but blocks #1952 live closure;
if the real coherent source still cannot fit, the incident remains open.

Managed wrapper or immutable hosted CI only. Independent code approval, normal
merge/updater rollout and replay of original PR1944 are separate exits. Source
issue1952 remains held until a reviewed implementation lane exists. Full
automatic capacity, productive owner CLI/Discord sessions, five autonomous
merges and fault/unattended acceptance remain open in #1703; no benchmark.

## Source baseline for implementation

The documentation worktree has an older source ancestor. Implement C1 from
current main, verified as416f392b17b56b2a43e67a3e430835902387941a at this
checkpoint, not by copying its source files. Primary source comparison finds
the relevant review-module delta is the wrapped typed-budget classifier
(#1885) and its regression tests. Preserve that classifier, its source-chain/
context handling and lookalike-string refusal. Other cited producer/parser/
episode/custody APIs were checked against the current deployment checkout.
A later main change requires the usual source revalidation before editing.

## Scoped design audit checkpoint

Fresh-reader: four initial forced-guess findings. Canonical selector JSON, raw
identifier equality and unavailable-source local control flow were resolved
and rechecked. The unknown PR1944 dependency pair was explicitly parked with
its first actual typed-response capture/replay trigger and live-closure cost;
no convenient pair is credited as actual incident evidence.

All32 mapped C1/boundary seams were checked. One stale source assertion (S23)
was corrected/rechecked: source-less adoption also reaches episode-evidence
persistence. Final result32 held, zero unresolved C1 seam findings. Primary
conversation/source comparison preserves the entire #1703 objective, current
main's wrapped-budget classifier and the C1/C2 staging boundary. Source/
component/rollout/live exits remain distinct. No implementation test or
independent code approval is implied by this audit.
