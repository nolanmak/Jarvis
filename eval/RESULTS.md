# Auto-PR scope eval

Cases: `eval/autopr-cases.json` · run 2026-09-16T02:51:18Z · **8/8 passed**

| # | Issue | Scoped at | Expected | Actual | Result | Notes |
|---|---|---|---|---|---|---|
| E989 | #989 | `3bcd247` | fixable | `fixable` | pass | complexity medium, est ~250 lines, 5 criteria |
| E962 | #962 | `350a2a3` | fixable | `fixable` | pass | complexity hard, est ~550 lines, 5 criteria |
| E968 | #968 | `967fcd1` | fixable | `fixable` | pass | complexity medium, est ~250 lines, 5 criteria |
| E963 | #963 | `d0db8e0` | fixable | `fixable` | pass | complexity medium, est ~300 lines, 4 criteria |
| E954 | #954 | `26b7df7` | fixable | `fixable` | pass | complexity hard, est ~450 lines, 5 criteria |
| E977 | #977 | `HEAD` | not-fixable | `not-fixable` | pass | refused: This issue is an acceptance checklist, not a code change: the implementation shipped and was verified in #976, and every unchecked item requires the owner to act interactively — authorizing … |
| E929 | #929 | `HEAD` | not-fixable | `not-fixable` | pass | refused: This is the umbrella tracking issue for an epic, and its actionable content is already decomposed into #926–#928 — the code shows #926 and #927 are substantially shipped (`high_confidence_me… |
| E882 | #882 | `HEAD` | not-fixable | `not-fixable` | pass | refused: This is a tracking/epic issue, not a unit of work: four of its six sub-issues (#883–#886) are already shipped — the `augmentagent-channel-imessage` crate with bundle reader, config, and sync… |

## What the grades mean
- **fixable**: the scoping pass judged the issue agent-fixable and the pipeline would build it.
- **not-fixable**: the scoping pass refused it, which for these fixtures is the correct call.
- **Scoped at**: the commit the working tree was placed at before scoping. `HEAD` means today's checkout.
- A miss on a `fixable` case is the expensive direction: shippable work the loop would quietly decline.

## Misses
- none
