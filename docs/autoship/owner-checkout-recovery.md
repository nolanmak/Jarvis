# Recover an issue after checkout continuity loss

`augmentagent autopr readopt --list --json` includes `continuity_lost`
candidates and a per-issue `action` command. Listing is read-only and provisional:
execution rechecks GitHub authorization, open state, manual routing, owner pause,
checkout custody and lease state. Intact checkouts are not recovery candidates.
Malformed or missing custody records do not authorize recovery.

A recognized acceptance hold or `agent-gave-up` label is not required. An
unpublished issue workflow at Intake, Scope, Build, Verify, Review or Reconcile
can be recovered when its recorded checkout continuity is lost. This includes
material-decision dependency waits and ordinary retry checkpoints. Intact
checkouts, attached PRs, publication/check/merge stages, explicit Held stages,
and terminal workflows retain their separate recovery paths. The command does
not turn any of those cases into a restart by changing their checkpoint.

Run the displayed command from the production controller checkout, with an owner
reason and a stable request ID. For example (synthetic repository):

```sh
augmentagent autopr readopt --repo example-owner/example-repo --issue 7 \
  --reason 'Restart from current main after checkout continuity loss' \
  --request-id owner-checkout-7-generation-1 --json
```

This command records authenticated owner authority and a durable, immutable
archive in `owner_checkout_recovery`. It retains the exact checkpoint TEXT,
original snapshot/context, original receipt path and bytes, and original readiness
dependency evidence. Existing scope responses, execution contexts, events,
receipt files, checkout and branch content remain untouched. The append-only
`superseded_by_owner_recovery` event links the prior checkpoint to its archive/request;
only the live workflow scheduling projection advances to Intake.

The next ordinary picker/claim uses a separate request-specific branch and
checkout, based on the GitHub main commit observed during owner recovery.
Preparation fetches main and creates that branch from the recorded base, never
from the old candidate. A new environment authority identity and domain-separated scope/readiness/budget
keys prevent old records from being reused as current verification. Current scope
readers and history scans honor the supersession event: malformed or missing
evidence and incomplete native admissions from before cutover remain archived,
with their charges intact. Missing responses or native custody after cutover still
fail closed. The pending
Intake gets one opportunity past old daily/dependency scheduling advice; ordinary
checks and backoff resume as soon as it advances. Recovery grants no build,
review, publication or merge approval. All normal execution gates still apply.

Retry the **same command and request ID** after an interrupted response. The
archive, authority, replacement path and base commit are committed together.
Replay reauthenticates and returns the original result without resetting a
progressed workflow or creating another checkout. Reusing the ID with different
intent is refused. A subsequent continuity loss needs a new owner request.

Owner pauses are preserved: explicitly resume an issue first only if that is the
owner's intent. Closed issues, `autoship-manual`, live leases and pending workflow
operations are refused. A separate GitHub `agent-gave-up` hold still follows the
existing legacy readoption rules; recovery does not silently remove that label.
Never delete the old receipt, reset the old branch, hand-edit the database, or
copy an unverified surviving branch into the new checkout to bypass a refusal.

## Fresh qualification after the owner cutover

Original-verifier index pages, direct verifier records and retained semantic
witness references honor the same supersession sequence as scope/native history.
Old rows and receipt files remain archived; they cannot force the new owner
episode to recover an unavailable original witness. Fresh scope establishes its
own baseline and qualification. Records created after the cutover still require
normal custody, budget accounting and witness validation.

The `fresh-witness-v2` owner qualification policy is part of the frontend
environment identity. A workflow which re-held under the earlier owner recovery
policy becomes eligible on normal picker ticks after this policy deploys, subject
to existing leases, owner pauses and routing gates. Do not repeat readopt or edit
state to clear that hold: its existing authority, request ID, archive and checkout
are retained. A missing witness from the new episode remains a hold.

## Automatic requalification of legacy recovered holds

An owner recovery that bounced to `acceptance_verifier_evidence_unknown` before
`fresh-witness-v2` now gets explicit priority at the next eligible issue-picker
turn, ahead of ordinary issues and original-evidence inspection hints. It uses
the existing per-issue owner request and replacement checkout. Today's refusal
ledger and a stale readiness dependency cannot suppress that first new-policy
turn. No repeated `readopt`, issue edit, environment change, or operator command
is needed after deployment.

The queue is finite: a protected context with any `owner-qualification:` version
means the new-policy attempt has started and consumes this legacy scheduling
override, including if it later has a real evidence failure. The picker still
checks current issue lifecycle/manual routing, author trust, owner pause,
lease/backoff, checkout continuity, and publication deduplication. Existing peer
fairness is preserved within the owner queue. Dry-run selection is read-only; normal
productive acquisition establishes the fresh environment, baseline and witness.
It does not declare a held workflow qualified or rewrite its original archive.

Unrecovered workflows, registered acceptance contexts, and current-policy custody
failures retain their existing protected recovery paths. The legacy scheduling
hint cannot manufacture owner authority or discard missing new evidence.

A real selection also records `owner_requalification_selected` under the active
owner request. It reserves scheduling only: no execution lease, model admission,
qualification or checkpoint update. If execution fails before claim, that issue
cools down for five minutes and yields to untouched owners/eligible peers. It
then retries automatically. Dry-run selection reads this schedule without
writing it. Changing to a new owner request uses a separate reservation history.

Owner priority is bounded (#2317). After three owner turns that leave the
workflow at the same scoping hold (no checkpoint past Scope since), the hold
loses its priority and rotates behind peers. Its cooldown doubles per further
turn, capped at six hours. Owner command jobs (`autopr run --issue N`) get the
same rule. Three consecutive attempts that end at the same stage, reason and
action double the command lane's retry delay each further time. Real progress,
a changed checkpoint, or a fresh owner submission restores ordinary turns.
