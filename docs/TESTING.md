# Where the test suites run

This page lists which suites run in CI, which run on a schedule, and which only
the owner runs. It also shows how to run the opt-in suites without touching live
daemon state. The Codex fallback contract they verify is in
[CODEX-FALLBACK.md](CODEX-FALLBACK.md).

## At a glance

| Suite | Where it runs | Needs |
|---|---|---|
| Python bridge, sandbox, VM-snapshot and dependency-proxy suites (`scripts/tests/*_test.py`) | CI, every PR and push to `main` | Nothing. Sandbox tests skip without Landlock ABI 6 |
| PR gate receipt rules (`scripts/tests/agent-pr-verify-gate.test.sh`) | CI, same job | `jq`, `git` |
| Privacy scripts, Node build and tests | CI (`privacy.yml`) | Nothing |
| Rust unit tests | Locally and in the auto-PR gate, per crate. Not in GitHub CI | `cargo` |
| Real-VM tests (Python and Rust) | Owner-run only | `JARVIS_TEST_VM_CONFIG` and a provisioned runtime |
| Live provider tests (Rust `#[ignore]`, `live_handoff_discovery_test.py`) | Owner-run only | Claude and/or Codex login. Uses provider quota |
| Nightly | Nothing is scheduled today | |

## CI: `Codex bridge suites`

`.github/workflows/bridge-suites.yml` has one job, `bridge-suites`. It runs on
`ubuntu-latest` for every pull request and every push to `main`:

1. Installs `poppler-utils` and `libseccomp2`.
2. Prints `python3 scripts/tests/host_capabilities.py`, which shows the kernel's
   Landlock ABI and whether the command sandbox and the PDF renderer are usable.
3. Fails if any of the four enforcement suites is missing:
   `codex_tool_bridge_test`, `codex_command_sandbox_test`, `codex_build_vm_test`
   and `build_dependency_proxy_test`.
4. From `scripts/`, runs `python3 -m unittest discover -s tests -p '*_test.py' -v`.
5. Runs `bash scripts/tests/agent-pr-verify-gate.test.sh`.

Tests that confine a real process (bridge `Bash` and build commands, PDF
rendering, `SandboxTests`) need Landlock ABI 6 (Linux 6.12+), `libseccomp.so.2`
and Poppler. On a host without them these tests **skip with that reason**, and
never fail. `host_capabilities.py` checks the host directly instead of calling
the sandbox, so a sandbox bug still fails on a host that supports it. The
hosted runner kernel reported Landlock ABI 7 when this job was added, so CI runs
every sandbox test. The capability step logs this on every run. If a future
runner image loses support, the step log shows it and those tests skip.

The only tests that skip in CI are the opt-in ones below: 5 `BuildVmTests`, 7
bridge `test_vm_*` tests and the live discovery test.

A failing test fails the job. `main` has no branch protection today, so the job
blocks a merge only after the owner marks the **`bridge-suites`** check as
required in the repository's branch protection or ruleset settings.

## Owner-run suites

These suites need private runtimes, provider logins or quota, so they never run
in CI. The PR gate (`scripts/agent-pr-verify-gate.sh`) asks for an owner-run
receipt before a PR that changes the code they cover can be opened. See
[Receipts](#receipts).

### Real-VM tests

```sh
export JARVIS_TEST_VM_CONFIG="$HOME/.local/share/augmentagent/build-vm/runtime.json"
cd scripts
python3 -m unittest tests.codex_build_vm_test tests.codex_tool_bridge_test -v
# Public registry probes additionally need:
JARVIS_TEST_PACKAGE_NETWORK=1 python3 -m unittest tests.codex_tool_bridge_test -v
```

Start one VM at a time. See [BUILD-VM.md](BUILD-VM.md#verification-and-rollback)
for provisioning and the Rust VM contract.

### Live provider tests (Rust)

Every live test is `#[ignore]`d and needs `--ignored`. Run one test or one file
at a time with a name filter, never the whole workspace:

| File | Providers and extras |
|---|---|
| `augmentagent-channel-core/src/handoff.rs` (`live_primary_*`) | Claude and Codex: the no-duplicate-effect invariant |
| `augmentagent-channel-core/src/codex.rs` | Codex. Some tests also need Poppler, `JARVIS_TEST_MEMORY_BIN`, the VM or public web |
| `augmentagent-channel-core/tests/provider_output_contracts.rs` | Claude or Codex. Some need Deno |
| `augmentagent-channel-core/tests/live_optional_mcp.rs` | Claude or Codex |
| `augmentagent-channel-discord-dm/src/digest.rs` | Claude or Codex |
| `augmentagent-cli/src/provider_channel_tests.rs`, `provider_migration_tests.rs` | Claude or Codex |
| `augmentagent-cli/src/main.rs` (`live_query_fallback_*`) | Codex and `JARVIS_TEST_MEMORY_BIN` |
| `augmentagent-cli/src/self_improve.rs` (`live_codex_*`) | Codex. The builder test also needs Claude and the VM |
| `augmentagent-cli/src/self_improve_lifecycle_tests.rs` | Codex, Claude and the VM: the full auto-ship lifecycle |

For example:

```sh
cargo test -p augmentagent-channel-core --lib handoff::tests::live_primary_receipt -- --ignored --nocapture --test-threads=1
cargo test -p augmentagent-cli --bins self_improve::lifecycle_tests -- --ignored --nocapture --test-threads=1
```

The Python live discovery test needs `JARVIS_TEST_LIVE_DISCOVERY=1` and a
Claude login.

## State isolation

The daemon keeps private state under one directory. Cooldown latches, handoff
journals, review history, tool-audit and token-usage logs, memory-nudge cycles
and the auto-PR ledgers all live there. The Rust code resolves it in one place,
`augmentagent_channel_core::state_dir`:

- `$XDG_STATE_HOME/augmentagent` when `XDG_STATE_HOME` is an absolute path,
- otherwise `$HOME/.local/state/augmentagent`.

The shell scripts use the same rule. Per-file overrides such as
`AUGMENTAGENT_COOLDOWN_FILE`, `AUGMENTAGENT_TOOL_AUDIT_LOG` and
`AUGMENTAGENT_TOKEN_USAGE_LOG` still win. Live tests can't just replace `HOME`,
because they need the real `~/.claude` and `~/.codex` logins. Replacing
`XDG_STATE_HOME` moves every state file and leaves the logins alone.

The test harnesses apply this override themselves:

- **`augmentagent-channel-core`'s own test binary** never uses the real
  directory. Without an explicit `XDG_STATE_HOME`, `state_dir()` returns a
  private scratch directory for the process. That covers the handoff and Codex
  live tests.
- **Live tests in other crates and in `channel-core/tests/`** call
  `augmentagent_channel_core::state_dir::isolate_for_tests()` before creating
  any reasoner. It points `XDG_STATE_HOME` at a private scratch directory,
  unless it already names a directory other than the real one. It panics if a
  per-file override still points into the real state directory.
- **Tests that re-run themselves as a child process** (the lifecycle, builder
  and query tests) set `XDG_STATE_HOME` inside their fixture directory. They
  also remove the per-file log overrides, so all state is deleted with the
  fixture.

Tests pin this behaviour:

- `state_dir::tests::every_state_path_and_write_follows_the_state_home_override`
  uses a synthetic `HOME`. It checks that every path and every real write lands
  under the override, and that nothing appears under `HOME`.
- `state_dir::tests::isolate_for_tests_moves_state_off_the_real_dir` checks the
  harness helper.
- `self_improve::tests::autopr_state_paths_follow_the_state_home_override`
  checks the auto-PR ledgers.
- `state_dir::tests::no_state_path_bypasses_the_shared_resolver` fails if any
  crate builds a `.local/state/augmentagent` path by hand.

For a manual live run, add your own fence as well. It costs nothing:

```sh
export XDG_STATE_HOME="$(mktemp -d)"
unset AUGMENTAGENT_COOLDOWN_FILE AUGMENTAGENT_TOOL_AUDIT_LOG AUGMENTAGENT_TOKEN_USAGE_LOG
cargo test -p <crate> <filter> -- --ignored --nocapture --test-threads=1
```

A before-and-after checksum of `~/.local/state/augmentagent` doesn't work on a
machine where the daemon is running, because the daemon keeps writing its own
logs and latches there. The synthetic-`HOME` pin test above is the reliable
check.

## Receipts

`scripts/agent-pr-verify-gate.sh` blocks `gh pr create` until
`.claude/agent-test-receipts/<HEAD-sha>.txt` exists when a PR changes one of
these scripts:

- `scripts/codex-tool-bridge.py`
- `scripts/codex-command-sandbox.py`
- `scripts/codex-build-vm.py`
- `scripts/build-dependency-proxy.py`
- `scripts/provider-supervisor.py`

CI can skip the sandbox tests, and it never runs the VM tests, so the receipt
records a run on a host that enforces the sandbox:

```text
command:      python3 scripts/tests/host_capabilities.py
              (cd scripts && python3 -m unittest discover -s tests -p '*_test.py' -v)
capabilities: the three capability lines ("command sandbox: enforceable", ...)
observed:     "Ran N tests" and "OK (skipped=K)", plus every skip reason
vm:           the JARVIS_TEST_VM_CONFIG run's result, or "not run: <reason>"
              (required for codex-build-vm.py, build-dependency-proxy.py and
              provider-supervisor.py)
verifies:     the changed script(s)
```

The gate's block message repeats these instructions. Receipts are keyed by HEAD,
so rebasing or amending means running the suites again.
