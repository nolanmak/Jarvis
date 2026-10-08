#!/usr/bin/env bash
# Deterministic Code Mode gates. Real VM/public registry acceptance stays in
# qa-code-mode-compute.py --require-vm --public-packages --cases all.
set -euo pipefail
cd "$(dirname "$0")/.."
export NO_COLOR=1
for suite in \
  crates/augmentagent-channel-core/tests/code_mode_compute.rs \
  crates/augmentagent-channel-core/tests/code_mode_runner.rs \
  crates/augmentagent-cli/tests/code_mode_compute.rs \
  crates/augmentagent-cli/tests/code_mode_dry_run.rs \
  crates/augmentagent-cli/src/compute_tool.rs \
  sidecars/code-mode-runner/runner_test.ts; do
  test -f "$suite" || { echo "missing required Code Mode suite: $suite" >&2; exit 1; }
done
logs=$(mktemp -d)
trap 'rm -rf "$logs"' EXIT
run_checked() {
  local pattern="$1"
  shift
  "$@" 2>&1 | tee "$logs/suite.log"
  grep -Eq "$pattern" "$logs/suite.log" || {
    echo "required Code Mode suite did not report any passing tests: $*" >&2
    return 1
  }
}
rust_result='^test result: ok\. [1-9][0-9]* passed; 0 failed;'
run_checked "$rust_result" cargo test -p augmentagent-channel-core --lib code_mode -- --test-threads=1
run_checked "$rust_result" cargo test -p augmentagent-channel-core --test code_mode_compute -- --test-threads=1
run_checked "$rust_result" cargo test -p augmentagent-channel-core --test code_mode_runner -- --test-threads=1
run_checked "$rust_result" cargo test -p augmentagent-cli --bin augmentagent compute_tool::tests -- --test-threads=1
run_checked "$rust_result" cargo test -p augmentagent-cli --test code_mode_compute -- --test-threads=1
run_checked "$rust_result" cargo test -p augmentagent-cli --test code_mode_dry_run -- --test-threads=1
run_checked '^ok \| [1-9][0-9]* passed \| 0 failed' deno test --no-lock --allow-run=deno --allow-read=. sidecars/code-mode-runner/runner_test.ts
