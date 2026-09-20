#!/usr/bin/env bash
# Gate configured public Git pushes on deterministic scanners, then CCat.
# A CCat review requires an operator-issued, single-use receipt bound to the
# exact local ref, remote ref, and redacted payload hash.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# Match the daemon's local-secret convention. This file is gitignored and is
# operator-owned; the gate never prints values loaded from it.
if [[ -f "$ROOT/.env" ]]; then
  set -a
  . "$ROOT/.env"
  set +a
fi
REMOTE_NAME="${1:?pre-push remote name missing}"
PUBLIC_REMOTES=",${AUGMENTAGENT_CCAT_PUBLIC_REMOTES:-},"

# Keep the hook inert until an operator explicitly declares a remote public.
[[ "$PUBLIC_REMOTES" == *",$REMOTE_NAME,"* ]] || exit 0

if [[ "${AUGMENTAGENT_CCAT_ENABLED:-false}" != "true" ]]; then
  echo "CCat is required for configured public remote '$REMOTE_NAME', but is disabled." >&2
  exit 1
fi
CALIBRATION_REPORT="${AUGMENTAGENT_CCAT_CALIBRATION_REPORT:-}"
if [[ -z "$CALIBRATION_REPORT" || ! -f "$CALIBRATION_REPORT" ]] \
  || ! python3 "$ROOT/scripts/ccat-calibrate.py" --input "$CALIBRATION_REPORT" >/dev/null; then
  echo "CCat public-push calibration is missing or does not meet its zero-false-allow threshold." >&2
  exit 1
fi

INPUT_FILE="$(mktemp "${TMPDIR:-/tmp}/augmentagent-ccat-push.XXXXXX")"
trap 'rm -f "$INPUT_FILE"' EXIT

run_ccat() {
  if [[ -n "${CCAT_BIN:-}" ]]; then
    "$CCAT_BIN" public-git-push "$INPUT_FILE"
  else
    (cd "$ROOT" && cargo run --quiet -p augmentagent-ccat --bin augmentagent-ccat -- public-git-push "$INPUT_FILE")
  fi
}

run_receipt_verify() {
  if [[ -n "${CCAT_RECEIPT_BIN:-}" ]]; then
    "$CCAT_RECEIPT_BIN" receipt-verify "$@"
  else
    (cd "$ROOT" && cargo run --quiet -p augmentagent-ccat --bin augmentagent-ccat -- receipt-verify "$@")
  fi
}

consume_receipt() {
  local receipt="${CCAT_APPROVAL_RECEIPT:-}"
  local payload_hash="$1"
  local local_sha="$2"
  local remote_ref="$3"
  [[ -n "$receipt" && -f "$receipt" ]] || return 1
  [[ -n "${XDG_STATE_HOME:-}" ]] || return 1
  run_receipt_verify "$receipt" "$payload_hash" "$local_sha" "$remote_ref" "$XDG_STATE_HOME"
}

while read -r local_ref local_sha remote_ref remote_sha; do
  # Deleted refs have no content to classify.
  [[ "$local_sha" =~ ^0+$ ]] && continue
  if [[ "$remote_sha" =~ ^0+$ ]]; then
    empty_tree="$(git hash-object -t tree /dev/null)"
    git diff --no-ext-diff --no-color --unified=0 "$empty_tree" "$local_sha" > "$INPUT_FILE"
  else
    range="$remote_sha..$local_sha"
    git diff --no-ext-diff --no-color --unified=0 "$range" > "$INPUT_FILE"
  fi

  # Existing deterministic protection runs before any provider request.
  if ! "$ROOT/scripts/check-no-personal-data.sh" "$INPUT_FILE" >&2; then
    echo "Public push blocked by local secret/PII checks; CCat was not called." >&2
    exit 1
  fi

  set +e
  result="$(run_ccat 2>&1)"
  status=$?
  set -e
  printf '%s\n' "$result" >&2
  case "$status" in
    0) ;;
    10)
      payload_hash="$(sed -n 's/.*payload_sha256=\([a-f0-9]\{64\}\).*/\1/p' <<<"$result" | head -n1)"
      if [[ -n "$payload_hash" ]] && consume_receipt "$payload_hash" "$local_sha" "$remote_ref"; then
        echo "CCat review receipt accepted for $remote_ref." >&2
      else
        echo "Public push requires a current CCAP receipt bound to this exact change." >&2
        exit 1
      fi
      ;;
    11) echo "Public push blocked by CCat." >&2; exit 1 ;;
    *) echo "CCat is unavailable; public pushes fail closed." >&2; exit 1 ;;
  esac
done
