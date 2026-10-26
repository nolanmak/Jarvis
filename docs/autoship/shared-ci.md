# Shared GitHub and Autoship CI commands

`scripts/ci-contract.json` owns validation command bodies. Every `run` step in
all 19 PR-validation workflows invokes `scripts/ci-contract.py step --hosted`
with its command ID. GitHub still owns its matrix, conditions, setup actions,
working directories and environment. The contract mechanically inventories that
metadata and binds exact workflow file hashes; it is not a second hand-written
selection of checks. Both `.yml` and `.yaml` files participate in inventory.
The scheduled scanner, issue implementation automation and advisory Claude
review are explicitly inventoried as non-validation workflows.

To change an existing command, edit its `commands` entry in the JSON. To add a
workflow/step, write its ordinary `run` body then run:

```sh
python3 scripts/ci-contract-sync.py
python3 scripts/ci-contract.py verify
python3 scripts/tests/ci_contract_test.py
```

The generator requires PyYAML; runtime planning/execution uses the standard
library. Generation preserves YAML metadata, extracts new command bodies, and
replaces inline runs with the shared invocation. Its reproducibility fixture
rejects unsynchronized metadata and new unclassified automation. Shared command
changes trigger every path-filtered validation workflow. Rust embeds the runner
and JSON contract; the updater rebuild predicate covers both files.

## Local controller interface

The trusted controller supplies its embedded runner and contract through
`python3 -I -c RUNNER --contract-json JSON`. It never selects a candidate-edited
manifest as authority. `plan --head HEAD --base BASE --root CHECKOUT --platform
linux --changed-file PATH --probe-prerequisites` returns raw-manifest SHA256,
bindings and ordered per-workflow/job/matrix checks. Repeat `--changed-file` for
all actual changed paths. An absent path list conservatively schedules all jobs.

Each check exposes its exact shell body, environment, working directory,
prerequisites and ID. Matrix variants have distinct IDs; `step --id ID
--input-head HEAD --input-base BASE --state-dir PRIVATE_STATE` re-derives the
same metadata from the trusted contract. Run steps from the candidate root,
inside the controller's existing resource/storage admission and filesystem/
network boundary. This runner is not itself an admission or isolation mechanism.
Do not run its build commands directly on the shared host outside managed
storage admission. The local controller must preserve step order and start each
workflow/job/matrix from a clean immutable candidate snapshot, as GitHub does.

Statuses have specific meanings:

- `ready`: local inputs/prerequisites are available; the command still must run.
- `prerequisite_hold`: recoverable missing tool/version/component/package; never
  success or a candidate-code failure. The controller must provision the declared
  dependency through its approved environment mechanism and retry.
- `remote_required`: foreign platform, privileged setup, unresolved GitHub
  context, or changed workflow authority. Exact-head hosted checks must supply
  the remaining evidence after push; this is not a permanent local shipping hold.
- `not_applicable`: declared path/OS condition does not apply.

Plan `complete` means no local prerequisite hold, **not** tests passed.
`remote_pending` separately records external obligations. Runtime and package
probes are read-only; no local `sudo`, apt/brew installation, or Playwright
browser/system installation is executed. Browser availability is a prerequisite,
not evidence a browser test passed. Unknown conditions remain external; matrix
`include` merging is explicitly rejected until supported rather than guessed.

Every local job gets an isolated `RUNNER_TEMP` under private controller state,
bound to checkout/head/base/job/matrix. The controller must allocate a fresh
state directory for every verification attempt, including retries; a failed job
restarts from its first step with a clean snapshot and state, never replaying a
failed command against partial artifacts. This retains scanner installs and other
cross-step artifacts. Only the currently declared WhatsApp binary-path export
may propagate through `GITHUB_ENV`; arbitrary control-environment overrides are
rejected. No host environment temporary directory is reused as job authority.

A candidate may legitimately amend CI. Workflow inventory drift adds an explicit
`contract-authority-change` remote requirement while retaining deployed trusted
checks locally. It neither executes candidate-amended check definitions as
controller authority nor permanently rejects workflow PRs. The updated contract
requires normal independent review and exact-head hosted inventory/CI evidence;
normal updater deployment then replaces the embedded version. A local pass
never waives mandatory hosted CI or proves macOS behavior from Linux.

The controller CI adapter (`scripts/ci-build-vm.py`) runs prerequisite probes and
commands inside the existing no-NIC build VM, with a pinned toolchain, read-only
Git object share, and private per-attempt job runtime. Candidate toolchain files
are not probed on the host. The controller supplies embedded runner/contract
bytes and actual inherited global plus target storage-lock descriptors. Cache
images reside inside the registered, leased build target; initial formatting is
fixed controller code, with private ownership, exact capacity and disk-growth
reserve checks. Missing guest tools and the unprovisioned pinned gitleaks release
are explicit hosted requirements, not successful local checks.

The adapter removes the build tool's arbitrary 900-second ceiling only for this
supervised CI mode. Its caller enforces declared GitHub step/job deadlines
(default job limit six hours), retains storage/resource supervision, and cleans
up the VM on cancellation. Lightweight adapter fixtures exercise this interface;
a real admitted VM run is still required before claiming deployment acceptance.

Targeted iteration and baseline diagnosis use fixed controller Cargo argv inside
the same VM. Package identifiers cannot begin with an option prefix; test filters
are bounded Rust paths. These diagnostic operations do not replace full CI.
Multi-repository owner-configured commands enter only through the private
controller specification and execute in guest Bash, never a host shell. CI guest
Cargo uses one build job, debug information disabled, and the existing registry
broker for dependencies; enabling registry access does not add a guest NIC or
general outbound access.

Real VM smoke (coordinated execution only): compile the ignored test through the
managed wrapper, let that build exit and release its lease, then invoke the
resulting **test executable**, not `cargo test`, under a delegated transient unit.
For the installed 4096 MiB build guest, the following reserves 5 GiB per run and
6 GiB for the enclosing unit. Set `CI_TEST_BINARY` to the centrally produced test
executable and `CI_REPO` to its trusted integration checkout. The evidence path
must not exist yet; the fixture creates it privately and retains protected audit
and a hashed diagnostic receipt.

```sh
systemd-run --user --wait --pipe --collect --unit=autoship-shared-ci-vm-smoke \
  -p 'Delegate=cpu memory pids' -p DelegateSubgroup=daemon \
  -p MemoryMax=6G -p TasksMax=512 -p CPUQuota=200% \
  --setenv=AUGMENTAGENT_RUN_RESOURCES=1 \
  --setenv=AUGMENTAGENT_RUN_MEMORY_BYTES=5368709120 \
  --setenv=AUGMENTAGENT_RUN_PIDS=256 \
  --setenv=AUGMENTAGENT_RUN_CPU_PERCENT=200 \
  --setenv=AUGMENTAGENT_BUILD_VM_CONFIG=/home/nolan-makatche/.local/share/augmentagent/build-vm/runtime.json \
  --setenv="AUTOSHIP_CI_ACCEPTANCE_REPO=$CI_REPO" \
  --setenv=AUTOSHIP_CI_ACCEPTANCE_EVIDENCE=/tmp/autoship-shared-ci-vm-smoke-evidence \
  "$CI_TEST_BINARY" --exact self_improve::shared_ci::acceptance::live_shared_ci_vm_smoke \
  --ignored --nocapture --test-threads=1
```

This exercises a real guest plan and the pinned Linux Python installer fixture,
with actual managed global/target descriptors and required resource admission.
Its explicit diagnostic path selection does not certify complete branch CI,
macOS checks, a full issue-to-main run, or deployment readiness.

The pinned command catalog also declares `vm_provisioning` requirements for
specific unsupported commands and their actual consumers. These currently keep
browser-sidecar PyPI/Playwright setup and smoke, PDF runtime setup/check/render
consumers, Docker image/compose probes, global Codex setup and its real redirect
probe, browser downloads/browser tests, and gitleaks scanning as hosted
obligations. No general outbound route is added. The local VM has only the
registry broker and resets guest HOME between steps; it cannot honestly claim
those environments are installed. Their independent mocked/unit/installer tests
remain local, including PDF runtime/updater fixtures and Codex redirect unit
tests. A future provisioner must supply pinned artifacts plus persistent private
job paths and verification before removing these explicit requirements. Direct
VM step invocation also rejects an unmet declaration before running its body.
