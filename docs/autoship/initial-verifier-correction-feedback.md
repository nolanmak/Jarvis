# Initial verifier correction feedback

An original verifier can return behavior that differs from its oracle while its
independent witness still finds the observation inadequate. The public failure
contains a safe summary. A fresh initial correction also needs the authenticated
private diagnosis to address the actual defect in its predecessor.

The correction reader selects the immediate failed proposal by its persisted
SHA-256, original source, complete workflow binding and baseline commit. It reads
the original retained proposal, registration and receipts. For a reproduced raw
result it requires a unique authenticated negative witness from the workflow's
existing native custody. Complete setup failures and other conclusive non-red
results can supply correction feedback without a semantic diagnosis. A missing
result is not a setup failure.

Before a fresh correction, all original verifier command admissions in that
workflow must have stored matching quiescence and a retained conclusive result or
an authenticated interrupted-attempt disposition. The reader cross-checks the
scope-record union against role indexes so losing an index does not hide earlier
work. Conclusive captures, when present, must be complete and match the receipt's
output digest. The existing legacy result-and-quiescence protocol remains
supported for admissions without capture data. An interrupted disposition can
settle an unrelated attempt; it cannot make the selected predecessor conclusive.
When recovery retains a later conclusive result for the exact same proposal,
earlier infrastructure results are excluded from selection after their command
custody and settlement checks pass. Infrastructure-only history still holds, and
conflicting conclusive results still hold.

Selection precedes checkout-dependent adapter checks. Retained adapters from
other proposals are not checked against the selected checkout. Record identity,
registration and command provenance checks still cover unrelated results. The
reader does not install adapters, run commands, quiesce processes or settle old
attempts.

The complete previous proposal, baseline and negative diagnosis are JSON-quoted
as untrusted data in the existing correction request. The entire added section,
including labels and escaping, is limited to 32 KiB of serialized UTF-8. Overflow,
missing custody and conflicting evidence hold with
`acceptance_verifier_evidence_unknown`; evidence is never silently truncated.
Private diagnosis does not enter public failure messages or grant implementation,
witness or merge authority. Existing provider request and source-selection limits
continue to apply separately.

A completed correction reply is validated and replayed before new evidence
inspection, including after restart. It does not require a new witness or model
call merely because the controller changed. The three-slot coordinator, durable
slot identities, budget references, prior decisions and charges remain intact.
This change does not resume paused or declined work.

The focused regressions run through the managed build wrapper:

```sh
scripts/cargo-managed.sh test -p augmentagent-cli --bin augmentagent -- initial_correction_
```

These component tests are separate from autoship acceptance. Repair PRs are
delivered as ordinary engineering work. An autonomous shipping result still
requires a naturally selected ordinary issue, qualified original behavioral red,
candidate green, independent review, exact-head CI and an autonomous merge.
