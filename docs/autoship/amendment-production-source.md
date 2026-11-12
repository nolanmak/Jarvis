# Construct verifier corrections from original production source

Implementation owner: the repair agent on #2050, under #1703/#1710/#1943.
This extends #2048 and `amendment-owner-evidence.md`; it does not replace the
original intent, witness, execution or merge authorities. Engineering delivery
and measured ordinary outcomes belong on #2050 and #1703.

## Observed failure

After #2049 deployed, ordinary #2014 generation 10 identified the obsolete
test-output-marker verifier but independently declined with no replacement
contract. The controller supplied complete owner intent and authenticated
negative diagnosis, but no original production-source excerpts. It requested
classification and described construction as following approval, while requiring
a complete replacement proposal in that same approved response. This establishes
missing inputs and ambiguous instructions, not a guarantee that a better-informed
authority will approve.

## Decisions

D1. Reuse the original protected native source-selection observation as data.
The existing WorkflowStore custody reader must correlate actual native admission,
accounting and response/provenance custody. The existing physical custody checker
must verify private files, byte hashes, provider/input correlation and separation
from worker-writable roots. Every proof must belong to the expected workflow
directory. Select exactly one source-selection observation at the registered
original commit, matching the authenticated negative judgment's context when
one is supplied. An amendment without a negative judgment may use the unique
original-commit source selection from that same custody. Require the unchanged
scope producer policy for direct selections, because its tracked-file ordinals
and selection schema are the frozen protocol being decoded. For the already
supported pre-deployment source policy, use only the existing version-1
`original-source-data-replay-v1` controller mapping. Bind its key to the admitted
current witness's exact context and original request hash; bind every original
field to the unique admitted old source observation's head, provider, input,
context, request, policy and supplied-request hash. The reader must reuse the
existing protected scope-response lookup semantics and reject missing recorded
mappings. This is a frozen data projection, never acceptance of an old judgment.
Do not accept a different context merely because its source files look useful.
Missing, ambiguous, foreign, malformed or
changed custody holds before paid admission. Do not edit the scope producer file
or accept arbitrary historical producer policies to make recovery pass.

D2. The controller reconstructs the same filtered immutable Git file index and
validates 1–16 integer selections, their 1–600-line ranges and optional bounded
literal needles. The existing credential exclusion owns the ordinal mapping;
the shared immutable-source reader additionally permits only admitted regular
tracked files. Paths never come from provider-supplied filesystem strings.
Reuse the existing immutable blob stream used by review source delivery, with
its fixed batches, offsets, UTF-8 and EOF checks. The provider gets no tools.
Preserve source whitespace exactly. Never read candidate working-tree bytes.

D3. Source delivery has a bounded serialized budget from the residual request
space, at most 384 KiB, and the entire final request remains within 512 KiB.
When all selected original files fit, supply their complete exact contents.
Otherwise supply bounded numbered excerpts derived from the retained selections
and explicitly label omitted bytes and empty searches unread. Partial source
never proves absence, qualification or complete source coverage. The controller
retains bounded source buffers while streaming, never an unbounded whole blob.
Overlong lines, oversized needed windows, binary/invalid UTF-8 and insufficient
remaining request space hold before paid admission; do not truncate silently or
spend a model call to rediscover the same failure. These holds require changed
trusted evidence or a future engineering correction; this amendment does not
invent a source-discovery model budget or automatic retry lane.

Fallback windows are mandatory for every retained selection: a range supplies
its inclusive 1-based lines through EOF; a needle supplies the first five literal
matches, each with up to 50 preceding and 99 following lines. Merge overlapping
lines within a selection and emit ascending numbered lines. Include every
selection's entry, even an empty search, labeled unread rather than absence.
Never omit a nonempty required window to fit the budget. If the union of required
windows or its serialized representation exceeds the residual budget, hold
before admission. Complete-file delivery can replace those windows when it fits.

D4. The independent authority is explicitly both correction proposer and
author. When evidence justifies an obsolete-verifier correction, construct its
complete next-revision contract and files in the current response. The original
proposal is the disputed artifact, not a proposed replacement to approve as-is.
Preserve every criterion ID and exact requirement, original base and implementation
scope. Reject unjustified amendments, missing dependencies and weakened/no-op
tests. A legitimate completed refusal remains final for unchanged inputs and
does not trigger schema correction. New original-base red cannot exist before
the new harness: approval permits staging and measurement, not activation,
candidate verification, final review or merging. All existing subsequent gates
must still pass with the newly generated behavioral correction.

D5. Bind the construction protocol version and exact source-evidence JSON digest
alongside the existing complete owner-request and negative-judgment identities in
the amendment input hash. Source evidence includes literal original commit,
authenticated selection identity, coverage label and exact supplied source.
After all bounded source/request checks, persist a private immutable version-2
amendment-context record with its context and source digests, then use the existing
two-per-input and four-per-total admission, RecoveryOnly binding and checkpoint.
Keep the existing source/original-base/execution-environment total epoch intact.
Declines, charges and uncertain outcomes survive; no new budget-reference system.

D6. Extend the existing free inspector/atomic semantic cue/normal claim path for
completed declines that have the exact known private version-1 owner-only context
record, its 64-hex context digest, and the matching original private completed
decline. Require the existing authenticated original Unknown witness, registered
contract and original request, plus a matching physically authenticated original
source selection. The synchronous inspector does not run Git or models; productive
preparation reconstructs actual source and checks the request bound before paid
admission. Reuse every existing source authorization, checkout/binding, pause,
owner revision, inbox, lease and unresolved-effect fence.

Use one stable explicit owner-and-source migration policy identity, independent
of build, day, generation and owner revision. The same existing atomic cue key,
bound to original judgment and that policy, permits at most one successor for
the historical omission. Preserve the previous owner-omission eligibility under
this stable migration while requiring source custody before queuing; do not make
arbitrary private context files eligible. Missing, malformed, ambiguous, future
or version-2 contexts do not qualify as known source omissions. The new complete
source decline remains held. A crash before cue commit is free inspection again;
after commit the existing Reconcile/source-continuation mechanism owns recovery,
including a crash before new context persistence. Once the amendment checkpoint
or actual native admission exists, existing custody rules handle its uncertain
result; never manufacture a completed refusal or new credit.

## Testable build order

1. Fail source-delivery and custody regressions before implementation. Then test
   original versus dirty candidate bytes, exact whitespace, namespace, tamper,
   context, ambiguous/unsupported producer, bounded windows and oversized input.
   Run an explicit ignored read-only production compatibility probe using actual
   retained #2014 custody; it may read Git and protected state, never write the
   workflow or make a model call.
2. Reproduce missing source/task delivery at the actual authority request boundary.
   Wire construction, exact evidence fingerprinting and pre-admission bounds;
   preserve valid denial and all downstream original-red/candidate-green gates.
3. Reproduce the actual known owner-only-context declined hold in the existing
   inspector/transaction/normal-selection tests. Verify one free continuation,
   original nonzero counters, unchanged-complete-source holds, all fences and
   each interruption boundary. Test missing/ambiguous data before paid admission.
4. Complete fresh-reader, conversation and every mapped seam audit; resolve
   findings. Run relevant/full managed CLI tests, exact-head hosted Linux checks
   and actual independent engineering review. Exclude the repair from autoship
   before it can execute, take the supported snapshot successfully, merge normally
   with current main (no rebase/force-push), and let the normal updater deploy.
5. Observe ordinary work naturally. No forced resubmission or fabricated owner
   input. Manual engineering merges count zero; #1703 remains open until all its
   original end-to-end, CLI/Discord, capacity and unattended acceptance exits hold.

## Non-goals, risks and open threads

No approval override, allowance reset, owner-intent change, source/SQLite edit,
baseline/candidate replacement, generic retry allocator, broader model tools,
benchmark or self-test with the repair PR. Selecting files is evidence discovery;
qualifying a measurement and approving a merge remain separate authorities.

Risk: the retained source selection may still omit a dependency. Coverage labels
must say so; the authority can decline. That does not justify claiming restoration
or retrying unchanged input. The repair owner must diagnose a concrete subsequent
refusal from evidence before proposing a further bounded change.

Open thread: verify the actual complete request fits with #2014's eight original
files (330,747 raw bytes), its original proposal and all existing evidence. This
blocks production rollout and is owned by the repair agent's read-only probe plus
actual request-boundary test. Component tests do not prove amendment approval or
an autonomous merge. For this measured historical case the rollout proof must
supply all eight complete files and record the actual serialized source and final
request sizes. A partial fallback remains supported generically but does not
satisfy this particular rollout proof. The first source-only probe correctly
refused the cross-context source selection; the stored existing projection must
be authenticated before this probe can pass.
New repair-PR exclusion must be proven through supported
controls before publishing a runnable main-based PR; a draft or issue label alone
does not fence adoption. This blocks publication, not local implementation.
