# Where the test suites run

This page lists which suites run in CI, which run on a schedule, and which only
the owner runs. It also shows how to run the opt-in suites without touching live
daemon state. The Codex fallback contract they verify is in
[CODEX-FALLBACK.md](CODEX-FALLBACK.md).

## At a glance

| Suite | Where it runs | Needs |
|---|---|---|
| Python bridge, sandbox, VM-snapshot and dependency-proxy suites (`scripts/tests/*_test.py`) | CI, every PR and push to `main` | Nothing. Locally, sandbox tests skip without Landlock ABI 6; in CI they fail |
| PR gate receipt rules (`scripts/tests/agent-pr-verify-gate.test.sh`) | CI, same job | `jq`, `git` |
| Privacy scripts, Node build and tests | CI (`privacy.yml`) | Nothing |
| Rust unit tests | Locally and in the auto-PR gate, per crate. Not in GitHub CI | `cargo` |
| Real-VM tests (Python and Rust) | Owner-run only | `JARVIS_TEST_VM_CONFIG` and a provisioned runtime |
| Live provider tests (Rust `#[ignore]`, `live_handoff_discovery_test.py`) | Owner-run only | Claude and/or Codex login. Uses provider quota |
| Nightly | Nothing is scheduled today | |

## CI: `Codex bridge suites`

`.github/workflows/bridge-suites.yml` has one job, `bridge-suites`. It runs on
`ubuntu-latest` for every pull request and every push to `main`:

1. Installs `poppler-utils`, `libseccomp2` and `ripgrep` (apt retries 3 times).
2. Prints `python3 scripts/tests/host_capabilities.py`, which shows the kernel's
   Landlock ABI and whether the command sandbox and the PDF renderer are usable.
   The job sets `REQUIRE_ENFORCEABLE_SANDBOX=1`, so this step fails when either
   is not.
3. Fails if any of the four enforcement suites is missing:
   `codex_tool_bridge_test`, `codex_command_sandbox_test`, `codex_build_vm_test`
   and `build_dependency_proxy_test`.
4. From `scripts/`, runs `python3 -m unittest discover -s tests -p '*_test.py' -v`.
5. Runs `bash scripts/tests/agent-pr-verify-gate.test.sh`.

Tests that confine a real process (bridge `Bash` and build commands, PDF
rendering, `SandboxTests`) need Landlock ABI 6 (Linux 6.12+), `libseccomp.so.2`
and Poppler. `host_capabilities.py` checks the host directly instead of calling
the sandbox, so a sandbox bug still fails on a host that supports it.

- **Locally** (switch unset), a host without them **skips** these tests with the
  reason.
- **In CI**, `REQUIRE_ENFORCEABLE_SANDBOX=1` turns each of those skips into a
  **failure**, and the capability step exits 1. A runner image that loses
  Landlock, libseccomp or Poppler fails the job instead of passing with no
  sandbox coverage. The hosted runner reported Landlock ABI 7 when this job
  was added.

`scripts/tests/host_capabilities_test.py` pins the switch with a faked report:
skip when it is off, failure when it is on.

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

Per-file overrides such as
`AUGMENTAGENT_COOLDOWN_FILE`, `AUGMENTAGENT_TOOL_AUDIT_LOG` and
`AUGMENTAGENT_TOKEN_USAGE_LOG` still win. Live tests can't just replace `HOME`,
because they need the real `~/.claude` and `~/.codex` logins. Replacing
`XDG_STATE_HOME` moves every state file the Rust code resolves and leaves the
logins alone.

Other readers of this state, and what cannot follow the rule:

| Reader | Follows `XDG_STATE_HOME`? |
|---|---|
| `scripts/lib/service-restart.sh` (updater): self-improve lane locks, restart stamps | Yes, the identical rule (`augmentagent_state_dir`). Pinned by `service-restart.test.sh` and `updater-restart-hygiene.test.sh` |
| `check-for-updates.sh` `built-commit` and `update.log`, `install-*.sh` log dirs | Yes, as `${XDG_STATE_HOME:-$HOME/.local/state}/augmentagent`. Same result unless the value is relative, which Rust ignores and these would use. Set it only to an absolute path |
| systemd `StandardOutput`/`StandardError` (`stdout.log`, `stderr.log`, the timer logs) | **No.** Fixed when the unit is written: `install-autostart.sh` expands `XDG_STATE_HOME` at install time, and `scripts/systemd/*.service` hardcode `%h/.local/state` or an absolute home. `autopr-health` reads `stderr.log` from the resolved state dir, so if `XDG_STATE_HOME` changes after install it looks in the wrong place |
| Legacy Node dashboard, `src/dashboard.ts` (`/api/audit`) | **No.** Reads `$HOME/.local/state/augmentagent/tool-audit.log` unless `AUGMENTAGENT_TOOL_AUDIT_LOG` is set |

None of this matters on the live host today: nothing sets `XDG_STATE_HOME`
there, so every reader uses `$HOME/.local/state/augmentagent`.

The test harnesses apply this override themselves:

- **`augmentagent-channel-core`'s own unit-test binary** uses a private
  scratch directory **only when `XDG_STATE_HOME` is unset**. That covers the
  handoff and Codex live tests in `src/`. If `XDG_STATE_HOME` is set, those
  tests use it as given, so don't point it at the real directory.
- **Integration tests (`channel-core/tests/`) and live tests in other crates**
  link the normal build and rely on `isolate_for_tests()`. They call
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
- `memory_nudge::tests::default_cycles_root_honors_xdg` checks the rule through
  the pure `state_dir::resolve()`.
- `scripts/tests/service-restart.test.sh` and
  `scripts/tests/updater-restart-hygiene.test.sh` check that the updater finds a
  lane lock held under `$XDG_STATE_HOME/augmentagent`.

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

CI runs the Python suites only after the PR exists and never runs the VM tests,
and a local host without Landlock skips the sandbox tests. So the receipt
records a run, before the PR, on a host that enforces the sandbox:

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
