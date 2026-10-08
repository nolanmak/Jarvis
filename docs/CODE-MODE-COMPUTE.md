# Code Mode compute

Code Mode can run Python with public wheel dependencies inside disposable KVM
guests. The operator enables it explicitly. The production tool is available only
to authorized owner Discord DM turns; the provider-free CLI uses the same compute
service. Other channel manifests keep their existing capabilities. Compute does
not send messages, publish artifacts, install host packages, or update the agent.

Issue [#1434](https://github.com/nolanmak/Jarvis/issues/1434) defines the release
contract. Before enabling a new build, require its complete current-commit
acceptance report as described in [TESTING.md](TESTING.md). A smoke run or passing
unit suite does not establish all isolation and cancellation requirements.

## Provisioning

Use an unprivileged Linux account with working KVM and the runtime described in
[BUILD-VM.md](BUILD-VM.md). Its QEMU, kernel, modules, static BusyBox and runtime
manifest must pass the existing runtime checks. Compute additionally requires
`virtio_console`, Python 3 in the trusted `/usr` runtime, the host `packaging`
parser, `openssl`, and `mke2fs`. Deno must support the pinned runner's import
restrictions; CI and acceptance development use Deno 2.7.14. The daemon account
must have persistent read/write access to `/dev/kvm`.

Configure these host-owned paths:

| Setting | Purpose |
| --- | --- |
| `AUGMENTAGENT_BUILD_VM_CONFIG` | Absolute path to the provisioned VM runtime JSON; defaults to `$HOME/.local/share/augmentagent/build-vm/runtime.json`. |
| `AUGMENTAGENT_BUILD_SCRATCH_DIR` | Private scratch root with sufficient capacity under the existing build admission policy. All compute callers must use the same root for global admission. |
| `AUGMENTAGENT_COMPUTE_PIP_RUNTIME` | Absolute path to the pinned guest installer manifest below; required when dependencies are requested. |
| `AUGMENTAGENT_DENO_BIN` | Optional absolute path to the operator-provisioned Deno binary. |

Provision an approved pip wheel as an operator-owned private regular file. Record
its independently verified SHA-256 in a private JSON manifest:

```json
{
  "path": "/absolute/private/runtime/pip-VERSION-py3-none-any.whl",
  "sha256": "REPLACE_WITH_THE_VERIFIED_64_CHARACTER_LOWERCASE_SHA256"
}
```

The placeholder is not a usable digest. The wheel must be at most 8 MiB and pass
the private-file checks; do not use a symlink. Keep the wheel and manifest readable
only by the runtime owner. This supplies the installer to the guest; do not run
`pip install` into the daemon environment. Dependency-free calls do not require
this manifest. A missing or invalid runtime fails closed; setting
`AUGMENTAGENT_BUILD_VM=host` does not enable a compute host fallback.

## Enablement and budgets

| Setting | Default | Valid values |
| --- | --- | --- |
| `AUGMENTAGENT_COMPUTE_ENABLED` | `false` | Boolean operator opt-in. |
| `AUGMENTAGENT_COMPUTE_TIMEOUT_SECS` | `600` | Integer seconds, 1–900; default and maximum for one compute call. |
| `AUGMENTAGENT_CODE_MODE_COMPUTE_TIMEOUT_SECS` | `1800` | Integer seconds, 1–3600; cumulative task budget shared across repair. |

Invalid values fail configuration validation. A model can request a smaller
per-call `timeoutSecs`, but cannot increase the operator maximum or reset the
task budget. Preparation, registry activity, VM startup, execution and export
consume the original monotonic deadline. Cleanup has at most five additional
seconds. Ordinary non-compute Code Mode keeps its 60-second default.

The requested dependency list accepts names and version comparisons, for example
`openpyxl==3.1.5` or `pandas>=2,<3`. Extras, direct URLs, paths, Git references,
private registries, editable installs, flags and direct environment markers are
rejected. Only wheels are installed. Transitive direct URLs are rejected too.
A successful resolution produces exact names, versions and wheel hashes.

Preparation runs without task inputs or code. Execution runs in a separate guest
with read-only dependencies and selected inputs, without the preparation gateway
or external network interface. Identical constraints reuse the successful
read-only environment only within the originating task, including repair.
Changed constraints prepare another environment. Failed preparation is never
reusable; a new task starts cold. Environments are removed when the task ends.

## Resource and artifact limits

| Resource | Limit |
| --- | --- |
| Guest CPUs / RAM | 2 vCPUs; validated runtime RAM between 512 and 4096 MiB. |
| Workload processes / calls | 128 processes; 25 compute calls per task. |
| Source / dependency count | 256 KiB UTF-8; 32 direct and 256 resolved packages. |
| Inputs | 32 files and 64 MiB total. |
| Outputs | 32 files; 32 MiB per file and 64 MiB total. |
| Captured logs | 8 MiB combined stdout/stderr; at most 64 KiB each in the public result. |
| Registry traffic per preparation | 32 MiB per response, 2,048 requests, 512 MiB total. |
| Concurrency | One compute VM per shared scratch root; contention is refused, not queued indefinitely. |

Guest tmpfs caps are 128 MiB for `/tmp`, 256 MiB for `/work`, 64 MiB for
`/outputs`, and 16 MiB for worker HOME. Guest-writable storage uses bounded
filesystems and the existing shared build scratch accounting. During dependency
preparation, an extracted wheel file may exceed the 32 MiB export ceiling; its
file-size ceiling is the bounded cache filesystem capacity. This allows native
libraries while keeping execution exports at 32 MiB per file. Retained task storage has a 2 GiB aggregate admission ceiling
and reserves 640 MiB per active task. Capacity refusals do not authorize bypassing
the VM or increasing limits from model arguments.

Inputs use opaque task-scoped artifact IDs. Filenames are flat ASCII names of at
most 128 characters; paths and duplicate names are refused. Work and output
directories do not implicitly persist between calls. To carry data forward,
request an output and select its returned artifact ID as the next call's input.

Only requested ordinary files can be exported. Export validation rejects links,
special files, traversal and size violations. Results are accepted after verified
guest shutdown. A failed call publishes no artifacts. Full bounded logs and audit
records remain private; treat them as sensitive task data.
Completed preparation phases, including failed installs, have a `preparation`
audit entry with download counts, elapsed time, cleanup status and private log
files named `audit-<executionId>-prepare.stdout` and `.stderr`. Their bytes and
SHA-256 are verified when the CLI copies the audit. Raw installer logs do not
appear in the public compute result. A reuse hit has `preparation: null` and
zero registry requests.
Each execution record also contains a canonical `policy` snapshot and its
`policyFingerprint`, covering host budgets, request limits and the embedded
compute/gateway helper hashes. `phases` records contiguous monotonic admission,
preparation, execution and export intervals for the work that actually occurred.
Compare these with the unchanged task deadline when diagnosing budget exhaustion.

Production artifacts belong to the originating account/task and expire after
24 hours if unclaimed. Startup recovery and the 30-second sweep remove expired
artifacts and verified orphan compute sessions, while preserving live owners.
CLI exports belong to the caller's explicit output directory and survive task
scratch cleanup. This command does not schedule deletion of those user exports.

## Provider-free CLI QA

Build and run from an isolated implementation worktree, with its own Cargo target.
Do not restart the live daemon or change its environment for QA. Set the runtime,
scratch and installer paths above, then write a TypeScript program such as:

```ts
async function main() {
  return await tools.compute.run({
    runtime: "python",
    dependencies: [],
    code: "from pathlib import Path\nPath('/outputs/total.txt').write_text(str(sum([10,20,30])))",
    outputs: ["total.txt"],
    timeoutSecs: 90,
  });
}
main();
```

Run with a new or empty destination and a report path that does not exist:

```sh
AUGMENTAGENT_COMPUTE_ENABLED=true \
  "$CARGO_TARGET_DIR/debug/augmentagent" code-mode compute-run \
  --program /absolute/private/example.ts \
  --inputs '{}' \
  --output-dir /absolute/private/new-exports \
  --report /absolute/private/new-report.json
```

`--inputs` is a JSON object mapping aliases to explicit local files. In the
program, `computeInputs.alias` is the opaque ID to use in
`inputs: [{artifactId: computeInputs.alias, name: "selected.txt"}]`. The guest
reads that selection at `/inputs/selected.txt`. It cannot derive host paths from
an artifact ID. Outputs appear under the supplied destination with no-clobber
creation; report publication failure rolls back newly copied outputs.

Exit 0 means the program and every compute call succeeded with verified cleanup.
Exit 1 means execution, policy or cleanup failed, even if the TypeScript program
handled a failed call. Exit 2 means invalid CLI arguments or configuration. The
JSON report contains the final value, execution records, artifact digests and
cleanup evidence. No provider credentials or live messaging tools are needed.

For acceptance, run the actual compiled binary through the harness:

```sh
python3 scripts/qa-code-mode-compute.py \
  --bin "$CARGO_TARGET_DIR/debug/augmentagent" \
  --vm-config "$AUGMENTAGENT_BUILD_VM_CONFIG" \
  --scratch-root "$AUGMENTAGENT_BUILD_SCRATCH_DIR" \
  --output-dir /absolute/private/new-qa-results \
  --require-vm --public-packages --cases all
```

Run real VM tests serially. The all-mode report must show all AC01–AC18 complete,
with actual command evidence and no missing or skipped required cases. Public
registry outages leave acceptance incomplete. Keep reports and logs private;
copy the successful report unchanged to the HEAD-specific receipt location in
[TESTING.md](TESTING.md). Rebuild and regenerate evidence after a rebase or code
change. A deployment is a separate operator action.

## Errors and rollback

| Error | Operator interpretation |
| --- | --- |
| `bad_args` | Invalid request schema; no VM or registry work should start. |
| `compute_disabled` | Context or operator policy does not permit compute. |
| `sandbox_unavailable` | Check KVM, runtime, private installer files and scratch prerequisites. |
| `dependency_policy_denied` | Requirement or registry access violates the wheel-only public policy. |
| `dependency_unavailable` | No usable wheel set could be prepared; inspect private evidence. |
| `dependency_integrity` | A dependency lock, hash or installed environment failed validation. |
| `input_denied` | Input capability or its checked snapshot is unavailable. |
| `timeout` / `cancelled` | The original deadline expired or the owning task was cancelled. |
| `resource_limit` | A configured resource or admission bound was reached. |
| `output_denied` | A requested export failed file/type/path validation. |
| `execution_failed` | Workload or orchestration failed; inspect bounded private logs. |
| `cleanup_unverified` | Cleanup could not be proved; no success may be published. Preserve evidence for recovery. |

For rollback, set `AUGMENTAGENT_COMPUTE_ENABLED=false` through the normal operator
configuration/deployment procedure. New owner turns stop receiving the compute
tool; existing non-compute capabilities remain available. A running turn retains
its task-owned context until completion or cancellation; changing configuration
is not a substitute for cancelling it. Keep recovery enabled and let verified
cleanup remove owned scratch. Do not manually delete live task directories or
kill processes based only on a recycled PID.
