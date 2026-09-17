#!/usr/bin/env bash
# #902 — the daemon's systemd unit must carry resource limits.
#
# On 2026-08-31 augmentagent.service fanned out ~300 `claude -p` children
# (#897). With MemoryMax=infinity the *kernel's* OOM killer picked victims
# across the whole login session — Chrome, the owner's Claude Code, dbus,
# then the X server — before it reached the daemon. A cgroup ceiling
# confines that kill to the unit. The 1024 soft fd limit is what happened to
# stop the fan-out at ~297; it must be an explicit number, and TasksMax must
# stop a fork storm well before the kernel does.
#
# Drives the real installers against a throwaway HOME with `systemctl` /
# `loginctl` stubbed, so no live unit is ever touched.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL %s\n     %s\n' "$1" "${2:-}"; }

# A fake repo the installers are happy with: they insist on an executable
# run-rs.sh and a release binary before writing anything.
make_case() {
  TMP=$(mktemp -d); export TMP
  mkdir -p "$TMP/repo/scripts" "$TMP/repo/target/release" "$TMP/bin" "$TMP/home"
  cp "$REPO_ROOT/scripts/install-autostart.sh" "$REPO_ROOT/scripts/install-tenant.sh" "$TMP/repo/scripts/"
  printf '#!/usr/bin/env bash\nexit 0\n' > "$TMP/repo/scripts/run-rs.sh"
  printf '#!/usr/bin/env bash\nexit 0\n' > "$TMP/repo/target/release/augmentagent"
  printf '#!/usr/bin/env bash\nexit 0\n' > "$TMP/bin/systemctl"
  # The unit cases render the Linux branch on any host (#1079: CI runs macOS too).
  printf '#!/usr/bin/env bash\necho Linux\n' > "$TMP/bin/uname"
  printf '#!/usr/bin/env bash\necho Linger=yes\n' > "$TMP/bin/loginctl"
  chmod +x "$TMP/repo/scripts/"*.sh "$TMP/repo/target/release/augmentagent" "$TMP/bin/"*
  UNIT="$TMP/home/.config/systemd/user/augmentagent.service"
  TENANT_UNIT="$TMP/home/.config/systemd/user/augmentagent-tenant-t1.service"
}

# run_installer <script> [args...] — env overrides come from the caller.
run_installer() {
  ( cd "$TMP/repo" \
    && PATH="$TMP/bin:$PATH" HOME="$TMP/home" XDG_CONFIG_HOME="$TMP/home/.config" \
       XDG_STATE_HOME="$TMP/home/.local/state" USER="${USER:-tester}" \
       "./scripts/$@" >"$TMP/install.out" 2>&1 )
}

has_line() { grep -qx -- "$2" "$1"; }

expect_directives() {
  local unit="$1" label="$2"
  for d in "MemoryHigh=8G" "MemoryMax=10G" "MemorySwapMax=0" "TasksMax=2048" "LimitNOFILE=4096" "OOMPolicy=continue"; do
    if has_line "$unit" "$d"; then ok "$label renders $d"; else bad "$label renders $d" "missing from $unit"; fi
  done
}

echo "install-autostart.sh unit resource limits (#902):"

# --- defaults -------------------------------------------------------------
make_case
if run_installer install-autostart.sh; then ok "installer runs against a throwaway HOME"
else bad "installer runs against a throwaway HOME" "$(tail -3 "$TMP/install.out")"; fi
[ -s "$UNIT" ] && ok "renders the unit" || bad "renders the unit" "no file at $UNIT"
expect_directives "$UNIT" "daemon unit"
has_line "$UNIT" "Restart=on-failure" && ok "keeps the existing Restart policy" \
  || bad "keeps the existing Restart policy" "Restart=on-failure gone"
# The limits belong to [Service], not [Unit]/[Install].
awk '/^\[Service\]/{s=1;next} /^\[/{s=0} s && /^MemoryMax=/{found=1} END{exit !found}' "$UNIT" \
  && ok "limits live in the [Service] section" || bad "limits live in the [Service] section" "MemoryMax not under [Service]"

if command -v systemd-analyze >/dev/null 2>&1; then
  VERIFY_OUT=$(systemd-analyze --user verify "$UNIT" 2>&1); rc=$?
  [ "$rc" -eq 0 ] && ok "systemd-analyze verify accepts the rendered unit" \
    || bad "systemd-analyze verify accepts the rendered unit" "exit $rc: $VERIFY_OUT"
  # verify only *warns* on an unparseable value; treat that as a failure too.
  if printf '%s\n' "$VERIFY_OUT" | grep -F "$(basename "$UNIT")" | grep -qiE "failed to parse|unknown key"; then
    bad "every rendered directive parses" "$(printf '%s\n' "$VERIFY_OUT" | grep -F "$(basename "$UNIT")")"
  else
    ok "every rendered directive parses"
  fi
else
  printf '  skip systemd-analyze not installed; verify step skipped\n'
fi
rm -rf "$TMP"

# --- overrides ------------------------------------------------------------
make_case
( export AUGMENTAGENT_UNIT_MEMORY_HIGH=9G AUGMENTAGENT_UNIT_MEMORY_MAX=12G \
         AUGMENTAGENT_UNIT_TASKS_MAX=1024 AUGMENTAGENT_UNIT_NOFILE=8192
  run_installer install-autostart.sh )
for d in "MemoryHigh=9G" "MemoryMax=12G" "TasksMax=1024" "LimitNOFILE=8192"; do
  has_line "$UNIT" "$d" && ok "override renders $d" || bad "override renders $d" "not found in $UNIT"
done
has_line "$UNIT" "MemoryMax=10G" && bad "override replaces the default (no duplicate MemoryMax)" "default still present" \
  || ok "override replaces the default (no duplicate MemoryMax)"
rm -rf "$TMP"

# --- dry-run override (#1079) ------------------------------------------------
make_case
run_installer install-autostart.sh
has_line "$UNIT" "ExecStart=$TMP/repo/scripts/run-rs.sh --wiki-dir ./wiki serve --dry-run false" \
  && ok "default unit still serves live (--dry-run false)" \
  || bad "default unit still serves live (--dry-run false)" "$(grep ExecStart "$UNIT")"
rm -rf "$TMP"
make_case
( export AUGMENTAGENT_AUTOSTART_DRY_RUN=true; run_installer install-autostart.sh )
has_line "$UNIT" "ExecStart=$TMP/repo/scripts/run-rs.sh --wiki-dir ./wiki serve --dry-run true" \
  && ok "AUGMENTAGENT_AUTOSTART_DRY_RUN=true renders --dry-run true" \
  || bad "AUGMENTAGENT_AUTOSTART_DRY_RUN=true renders --dry-run true" "$(grep ExecStart "$UNIT")"
rm -rf "$TMP"
make_case
( export AUGMENTAGENT_AUTOSTART_DRY_RUN=maybe; run_installer install-autostart.sh ) \
  && bad "rejects a dry-run value that is not true/false" "installer exited 0" \
  || ok "rejects a dry-run value that is not true/false"
rm -rf "$TMP"

# --- macOS launchd agent (#1079) ---------------------------------------------
echo "install-autostart.sh launchd plist (#1079):"
make_case
printf '#!/usr/bin/env bash\necho Darwin\n' > "$TMP/bin/uname"
printf '#!/usr/bin/env bash\necho "$*" >> "%s/launchctl.calls"\n[ "$1" = print ] && exit 113\nexit 0\n' "$TMP" > "$TMP/bin/launchctl"
chmod +x "$TMP/bin/uname" "$TMP/bin/launchctl"
PLIST="$TMP/home/Library/LaunchAgents/com.nolanmak.augmentagent.plist"
if ( export AUGMENTAGENT_AUTOSTART_DRY_RUN=true; run_installer install-autostart.sh ); then
  ok "installer runs on (stubbed) Darwin"
else
  bad "installer runs on (stubbed) Darwin" "$(tail -3 "$TMP/install.out")"
fi
[ -s "$PLIST" ] && ok "renders the launchd plist" || bad "renders the launchd plist" "no file at $PLIST"
grep -q "<string>$TMP/home/.local/state/augmentagent/stderr.log</string>" "$PLIST" \
  && ok "plist logs to the shared state dir (autopr-health reads it there)" \
  || bad "plist logs to the shared state dir (autopr-health reads it there)" "$(grep -A1 StandardErrorPath "$PLIST")"
awk '/<string>--dry-run<\/string>/{getline; print; exit}' "$PLIST" | grep -q '<string>true</string>' \
  && ok "plist carries the dry-run override" || bad "plist carries the dry-run override" "not true"
grep -q "^bootstrap gui/" "$TMP/launchctl.calls" && ok "bootstraps the agent" \
  || bad "bootstraps the agent" "calls: $(cat "$TMP/launchctl.calls" 2>/dev/null)"
if command -v plutil >/dev/null 2>&1; then
  plutil -lint "$PLIST" >/dev/null 2>&1 && ok "plutil -lint accepts the plist" \
    || bad "plutil -lint accepts the plist" "$(plutil -lint "$PLIST" 2>&1)"
else
  printf '  skip plutil not installed; lint step skipped\n'
fi
rm -rf "$TMP"

# --- tenant unit ------------------------------------------------------------
echo "install-tenant.sh unit resource limits (#902):"
make_case
if run_installer install-tenant.sh t1; then ok "tenant installer runs against a throwaway HOME"
else bad "tenant installer runs against a throwaway HOME" "$(tail -3 "$TMP/install.out")"; fi
[ -s "$TENANT_UNIT" ] && ok "renders the tenant unit" || bad "renders the tenant unit" "no file at $TENANT_UNIT"
expect_directives "$TENANT_UNIT" "tenant unit"
rm -rf "$TMP"

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
