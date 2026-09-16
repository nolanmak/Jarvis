# Auto-PR scope eval

Cases: `eval/autopr-cases.json` · run 2026-09-16T01:36:20Z · **8/8 passed**

| # | Issue | Scoped at | Expected | Actual | Result | Notes |
|---|---|---|---|---|---|---|
| E989 | #989 | `3bcd247` | fixable | `fixable` | pass | complexity medium, est ~320 lines, 5 criteria |
| E962 | #962 | `350a2a3` | fixable | `fixable` | pass | complexity hard, est ~500 lines, 5 criteria |
| E968 | #968 | `967fcd1` | fixable | `fixable` | pass | complexity medium, est ~280 lines, 5 criteria |
| E963 | #963 | `d0db8e0` | fixable | `fixable` | pass | complexity medium, est ~250 lines, 5 criteria |
| E954 | #954 | `26b7df7` | fixable | `fixable` | pass | complexity hard, est ~450 lines, 5 criteria |
| E977 | #977 | `HEAD` | not-fixable | `not-fixable` | pass | refused: This issue is an owner-action acceptance checklist, not a coding task: the implementation already exists and is merged (`crates/augmentagent-finance/` with connect/client/statements/export m… |
| E929 | #929 | `HEAD` | not-fixable | `not-fixable` | pass | refused: This is a tracking epic, not a shippable unit: its completion is the closure of its dependency-ordered children (#926–#928), not a single focused diff, and a build against the tracker would … |
| E882 | #882 | `HEAD` | not-fixable | `not-fixable` | pass | refused: This is a tracking/epic issue, not a unit of implementable work: four of its six sub-issues (#883–#886) are already shipped (the `augmentagent-channel-imessage` crate, sync CLI, and ingest k… |

## What the grades mean
- **fixable**: the scoping pass judged the issue agent-fixable and the pipeline would build it.
- **not-fixable**: the scoping pass refused it, which for these fixtures is the correct call.
- **Scoped at**: the commit the working tree was placed at before scoping. `HEAD` means today's checkout.
- A miss on a `fixable` case is the expensive direction: shippable work the loop would quietly decline.

## Misses
- none
