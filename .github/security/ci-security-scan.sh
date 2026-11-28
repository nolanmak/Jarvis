#!/bin/bash
# The security scans a CI job runs, one policy for every repo. Copied into a
# repo at .github/security/ by github-ci-security's apply.sh; install the tools
# with install-tools.sh first, then run it from the root of a checkout.
#
#   ci-security-scan.sh                 gitleaks osv shai-hulud semgrep zizmor
#   ci-security-scan.sh osv zizmor      only these
#
# Every scan runs even if an earlier one fails, so one red never hides another.
# What fails the job:
#   gitleaks    any finding (history of HEAD, so the checkout needs fetch-depth: 0)
#   osv         any known vulnerability not ignored in the repo's osv-scanner.toml
#   shai-hulud  HIGH findings; MEDIUM is listed for a person to read
#   semgrep     ERROR severity; WARNING and INFO are listed only
#   zizmor      High severity; lower is listed only
#   any         a scanner that is missing or crashes
#
# SCAN_REPORT_ONLY  tools that list findings but never fail the job, e.g. while a
#                   repo clears its backlog: SCAN_REPORT_ONLY="semgrep zizmor"
# SEMGREP_RULES     the Semgrep config, default p/default
# GH_TOKEN          when set, zizmor also runs its online audits
# shellcheck disable=SC2317,SC2329  # the scan_* functions are called by name below
set -uo pipefail

ALL_TOOLS="gitleaks osv shai-hulud semgrep zizmor"
# install-tools.sh exports SHAI_HULUD; the fallback is where it installs it.
SHAI_HULUD=${SHAI_HULUD:-${RUNNER_TEMP:-/tmp}/security-tools/shai-hulud-detect/shai-hulud-detector.sh}
SEMGREP_RULES=${SEMGREP_RULES:-p/default}
SCAN_REPORT_ONLY=${SCAN_REPORT_ONLY:-}
work=$(mktemp -d); trap 'rm -rf "$work"' EXIT

CLEAN=0 FINDINGS=1 BROKEN=2 NOTHING=3

scan_gitleaks() {
  if [ "$(git rev-parse --is-shallow-repository 2>/dev/null)" != false ]; then
    echo "gitleaks needs full history: set fetch-depth: 0 on actions/checkout"
    return "$BROKEN"
  fi
  # HEAD's history, not every branch: one branch's fixture must not fail every
  # other PR (Foss-app#1491). --redact keeps the secret itself out of the log.
  gitleaks git --redact -v --no-banner --log-opts=HEAD --exit-code=99 .
  case $? in
    0) return "$CLEAN" ;;
    99) return "$FINDINGS" ;;
    *) return "$BROKEN" ;;
  esac
}

scan_osv() {
  osv-scanner scan source -r .
  case $? in
    0) return "$CLEAN" ;;
    1) return "$FINDINGS" ;;
    128) return "$NOTHING" ;;  # no lockfile in this repo
    *) return "$BROKEN" ;;
  esac
}

# The detector flags any workflow with both `runs-on: self-hosted` and
# ${{ github.event.*.body }}, which is what the worm's backdoor looks like, and
# has no allowlist. On our runners that also matches `PR_BODY: ${{
# github.event.pull_request.body }}` in an env block, which is the safe way to
# read a PR body. True only when every such reference in the file is that shape;
# a discussion or issue body, or a PR body inside a run: script, stays HIGH.
only_env_pr_body() {
  ! grep -E '\$\{\{ *github\.event\.[^}]*\.body *\}\}' "$1" \
    | grep -qvE '^[[:space:]]*[A-Za-z_][A-Za-z0-9_]*:[[:space:]]*\$\{\{ *github\.event\.pull_request\.body *\}\}[[:space:]]*$'
}

scan_shai_hulud() {
  local report=$work/shai.json high=0 file message
  "$SHAI_HULUD" --json "$report" .
  [ -s "$report" ] || return "$BROKEN"
  while IFS=$'\t' read -r file message; do
    if [[ $message == *"Self-hosted runner with dynamic payload"* ]] && [ -f "$file" ] && only_env_pr_body "$file"; then
      echo "shai-hulud: ignoring $file: its only event body is a PR body read through env"
      continue
    fi
    echo "shai-hulud HIGH: $file: $message"
    high=$((high + 1))
  done < <(jq -r '.findings[] | select(.severity == "HIGH") | [.file, .message] | @tsv' "$report")
  [ "$high" -eq 0 ] || return "$FINDINGS"
}

scan_semgrep() {
  local report=$work/semgrep.json blocking
  semgrep scan --config "$SEMGREP_RULES" --metrics=off --disable-version-check \
    --text --json-output="$report" . || return "$BROKEN"
  blocking=$(jq '[.results[] | select(.extra.severity | IN("ERROR", "HIGH", "CRITICAL"))] | length' "$report") || return "$BROKEN"
  [ "$blocking" -eq 0 ] || return "$FINDINGS"
}

scan_zizmor() {
  zizmor --no-progress .
  case $? in
    0|10|11|12|13) return "$CLEAN" ;;  # 10 to 13: findings below High
    14) return "$FINDINGS" ;;
    3) return "$NOTHING" ;;            # no workflows in this repo
    *) return "$BROKEN" ;;
  esac
}

binary_of() {
  case $1 in
    gitleaks) echo gitleaks ;;
    osv) echo osv-scanner ;;
    shai-hulud) echo "$SHAI_HULUD" ;;
    semgrep) echo semgrep ;;
    zizmor) echo zizmor ;;
  esac
}

tools=${*:-$ALL_TOOLS}
for tool in $tools; do
  case " $ALL_TOOLS " in
    *" $tool "*) ;;
    *) echo "ci-security-scan: unknown tool '$tool' (known: $ALL_TOOLS)" >&2; exit 2 ;;
  esac
done

failed=0
table="| Scan | Result | Time |
|---|---|---|
"
for tool in $tools; do
  echo "::group::$tool"
  started=$SECONDS
  if command -v "$(binary_of "$tool")" >/dev/null; then
    "scan_${tool//-/_}"; rc=$?
  else
    echo "$(binary_of "$tool") is not installed"; rc="$BROKEN"
  fi
  echo "::endgroup::"
  case $rc in
    "$CLEAN") result=clean ;;
    "$NOTHING") result="nothing to scan" ;;
    "$FINDINGS") result=findings ;;
    *) result="scan failed" ;;
  esac
  if [ "$rc" = "$FINDINGS" ] || [ "$rc" = "$BROKEN" ]; then
    case " $SCAN_REPORT_ONLY " in
      *" $tool "*) result="$result (report only)" ;;
      *) failed=1 ;;
    esac
  fi
  table+="| $tool | $result | $((SECONDS - started)) s |
"
done

printf '%s' "$table"
[ -z "${GITHUB_STEP_SUMMARY:-}" ] || printf '%s' "$table" >> "$GITHUB_STEP_SUMMARY"
exit $failed
