# Stale PR sweep

Every two days the autoship worker looks at each open pull request that has had
no activity for two days and decides whether it should be merged, built out or
closed. Before this, a PR stacked on a branch that had already merged, or one
labelled `autoship-manual`, was never picked up again.

## Which PRs

Same-repository PRs by a trusted author whose `updatedAt` is at least
`AUTOSHIP_STALE_PR_DAYS` old, oldest number first, at most 6 per pass. Forks
and untrusted authors are ignored, as for adoption.

## Outcomes

| Outcome | When | What the sweep does |
| --- | --- | --- |
| Close | The head has no commit that `main` lacks | Closes with a comment. No model call. |
| Close | The reviewer says CLOSE and the PR is conflicting or has a failing check | Closes with the reviewer's reason. |
| Merge / continue build | The reviewer says MERGE or CONTINUE | Hands the PR to the adopted-PR pipeline. |
| Leave | Stacked on an open parent, base not landed, or already routed | Logs the reason. No write. |

A mergeable PR with no failing check is never closed on the reviewer's opinion.
The branch of a closed PR is kept, so reopening restores it.

The sweep does not merge. Handing a PR over means removing what kept it out of
automatic routing:

- a base branch whose commits are all on `main` is retargeted to `main`;
- the PR's own `autoship-manual` label is removed.

The adopted-PR pipeline then re-verifies the PR on current `main`, repairs it,
reviews it and merges it on LGTM, under the same approval rule as every other
autoship merge. A source issue that carries `autoship-manual` still holds its
PR; the sweep never edits issue labels.

## Settings

| Variable | Default | Meaning |
| --- | --- | --- |
| `AUTOSHIP_STALE_PR_DAYS` | `2` | Idle threshold and pass interval, in days. `0` disables the sweep. |
| `AUTOSHIP_STALE_PR_RELEASE_MANUAL` | on | `0` keeps `autoship-manual` PRs manual: the sweep leaves them alone. |

## Scheduling and spend

The sweep shares the hourly scheduler with the nightly review. A pass is due
when the interval has passed since the last completed pass. The reviewer lease
is refused while a shipping workflow is active, so a due pass that was deferred
retries every 5 minutes until it runs. A pass is skipped while
the global model budget is exhausted, and makes one reviewer call per PR that
still has commits to land. State is in `autoship-stale-pr-sweep.json` in the
state directory. A pass in which every PR failed is not recorded as done and
retries on a later tick, at most four times a day.
