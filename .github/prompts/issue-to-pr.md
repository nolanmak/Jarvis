# Issue-to-PR autonomous implementation prompt

You are implementing the fix described in the GitHub issue passed as `ISSUE_NUMBER`. This is a Rust workspace (`Cargo.toml` at repo root, crates under `crates/`).

## Workflow

1. Run `gh issue view $ISSUE_NUMBER` to read the issue body.
2. Read relevant source files. Prefer reading whole crate roots before editing.
3. Make the minimal change that satisfies the issue's acceptance criteria. Do **not** refactor adjacent code, add features that weren't asked for, or change public APIs unless the issue requires it.
4. Run `cargo check -p <crate>` on each crate you touched. Do **not** run `cargo check --workspace` (too slow, RAM-heavy).
5. Run `cargo clippy -p <crate> -- -D warnings` on each touched crate. Fix any warnings you introduced. Do **not** touch warnings that existed before your change.
6. Run `cargo test -p <crate>` on each touched crate. All tests must pass.
7. Commit with a clear message in the repo's convention: `feat(scope): subject` or `fix(scope): subject`. **Never** add `Co-Authored-By` trailers or `Generated with Claude Code` lines — repo convention forbids them.
8. Open a draft PR using `gh pr create --draft --base main` with title matching the commit, body listing the closed issue (`Closes #N`), the changed files, and a "Test plan" checklist with what you actually ran.

## Protected paths

The contents of `PROTECTED_PATTERNS` (passed in env) are off-limits. If your fix would require editing any path matching those patterns, **stop** and add a comment on the issue explaining what's needed — do not attempt a workaround. The post-implementation guardrails will revert any edit to a protected path and the PR will be auto-closed.

## Out-of-scope behavior

If the issue is ambiguous, post a clarifying comment on the issue and exit without opening a PR. Do **not** guess.

If the issue's acceptance criteria include manual smoke-tests, network calls, or credentials you don't have, implement the structural changes only and call out the gap explicitly in the PR body as "Manual verification required: …".

## Style

- No comments unless WHY is non-obvious (per repo `CLAUDE.md`).
- No new files for documentation unless the issue asks for them.
- No `// removed`, `// TODO`, or dead-code stubs.
- Tests live next to the code they test (`#[cfg(test)] mod tests`) unless the crate uses a `tests/` dir.
