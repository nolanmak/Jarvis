# Ordinary peer selection after an unchanged failure (#1706)

The observation-wake regression now connects `AutoPrLoop::run_builds` to the
production `select_issue_with_fallback` function. The existing subprocess fixture
supplies temporary Git repositories, two native WorkflowGuard input holds, and
a scripted GitHub inventory with an older eligible ordinary issue.

Both retained workflows remain unchanged and no qualification-recovery cue is
present. After a failed worker turn, the real selector must return ordinary issue
1829 within two five-minute observation cycles. The test verifies that neither
held workflow is reclaimed or charged, that their snapshots and semantic events
are preserved, and that only unpaid observation metadata can be appended. It
checks zero historical model calls, no new workflow for the selected issue, no
remote edit/comment/merge operation, and unchanged merge allowance of five.

Worker scheduling uses fake time. The test restores real time while the selector
runs external Git/GitHub fixture processes, so an artificially expired IO timeout
cannot supply failure evidence. Child processes isolate the GitHub/state fixtures
from the owner's environment and from other tests.

Replay before the #2124 observation-wake fix fails because the real ready peer is
never selected within the allowed interval. The identical corrected fixture
passes with that fix. The initial fixture accidentally recovered another issue;
that first failure is a setup error and is excluded from product RED evidence.

This proves selection through the native scheduler/component boundary. It does
not execute a provider, write a candidate, open/review/merge a PR, establish live
shipping or close #1706. Production end-to-end acceptance remains separate.
