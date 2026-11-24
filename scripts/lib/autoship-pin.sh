#!/usr/bin/env bash
# #2468 — the auto-ship worker runs a binary built from a pinned commit, not
# from main's head. Main keeps deploying everything else every five minutes;
# the worker's executable changes only when the owner moves the pin.
#
# Files (all owned by this user):
#   pin    ${XDG_CONFIG_HOME:-~/.config}/augmentagent/autoship-pin
#          The owner's choice: a full commit SHA, or "main" (unpinned: the
#          worker follows main deploys again, the pre-#2468 behaviour).
#          Written only by scripts/autoship-pin.sh and the first-deploy
#          bootstrap below.
#   stamp  ${XDG_STATE_HOME:-~/.local/state}/augmentagent/autoship-pinned-built
#          The commit the installed pinned binary was built from ("main" after
#          an unpin). The worker watches this file instead of built-commit
#          (AUGMENTAGENT_AUTOSHIP_DEPLOY_STAMP), so a main deploy is not a
#          reason for it to reload.
#   root   /mnt/build/augmentagent-autoship-pinned (src/, target/, bin/).
#          Build output stays off the root filesystem. Override with
#          AUGMENTAGENT_AUTOSHIP_PIN_ROOT.
#
# Callers provide log(); the updater also provides run_update_build,
# DAEMON_BINARY and REPO_ROOT. Pinning is on by default on Linux (the deploy
# host); AUGMENTAGENT_AUTOSHIP_PIN=on|off overrides that.

autoship_pin_file() { printf '%s/augmentagent/autoship-pin\n' "${XDG_CONFIG_HOME:-$HOME/.config}"; }
autoship_pin_stamp() { printf '%s/augmentagent/autoship-pinned-built\n' "${XDG_STATE_HOME:-$HOME/.local/state}"; }
autoship_pin_root() {
  if [ -n "${AUGMENTAGENT_AUTOSHIP_PIN_ROOT:-}" ]; then
    printf '%s\n' "$AUGMENTAGENT_AUTOSHIP_PIN_ROOT"
  elif [ -d /mnt/build ] && [ -w /mnt/build ]; then
    printf '%s\n' /mnt/build/augmentagent-autoship-pinned
  else
    printf '%s/augmentagent/autoship-pinned\n' "${XDG_CACHE_HOME:-$HOME/.cache}"
  fi
}
autoship_pin_binary() { printf '%s/bin/augmentagent\n' "$(autoship_pin_root)"; }

autoship_pin_enabled() {
  case "${AUGMENTAGENT_AUTOSHIP_PIN:-}" in
    on) return 0 ;;
    off) return 1 ;;
  esac
  [ "$(uname -s)" = Linux ]
}

# Print the recorded pin: a full SHA, "main", or nothing when none is
# recorded. Fails on anything else, so a typo never selects a build.
autoship_pin_read() {
  local value=""
  [ -e "$(autoship_pin_file)" ] || return 0
  value=$(tr -d '[:space:]' < "$(autoship_pin_file)") || return 1
  case "$value" in
    ''|main) printf '%s' "$value" ;;
    *) [[ "$value" =~ ^[0-9a-f]{40}$ ]] || return 1; printf '%s' "$value" ;;
  esac
}
autoship_pin_installed() { tr -d '[:space:]' < "$(autoship_pin_stamp)" 2>/dev/null || true; }

# Whether the worker unit should run the pinned binary: one is installed and
# the owner has not unpinned. A pin that has moved but is still building is
# ready too; the worker stays on the previous pinned binary meanwhile.
autoship_pin_ready() {
  local pin installed
  pin=$(autoship_pin_read 2>/dev/null) || return 1
  installed=$(autoship_pin_installed)
  [ "$pin" != main ] && [[ "$installed" =~ ^[0-9a-f]{40}$ ]] && [ -x "$(autoship_pin_binary)" ]
}

# The updater's decision, kept free of side effects so it is tested alone.
#   autoship_pin_decision PIN INSTALLED BINARY_PRESENT(0|1) BOOTSTRAP
# Prints one of:
#   none       nothing recorded and nothing to bootstrap from, or unpinned
#   bootstrap  no pin yet: record BOOTSTRAP as the pin, then install it
#   unchanged  the installed binary is the pin; no rebuild, no worker reload
#   install    the pin moved (or the binary is missing): rebuild and reload
#   unpin      the owner chose "main" while a pinned binary is installed
autoship_pin_decision() {
  local pin="$1" installed="$2" present="$3" bootstrap="$4"
  case "$pin" in
    '') if [ -n "$bootstrap" ]; then echo bootstrap; else echo none; fi ;;
    main) if [[ "$installed" =~ ^[0-9a-f]{40}$ ]]; then echo unpin; else echo none; fi ;;
    *) if [ "$installed" = "$pin" ] && [ "$present" = 1 ]; then echo unchanged; else echo install; fi ;;
  esac
}

autoship_pin_write() {
  local file="$1" value="$2" dir tmp
  dir=$(dirname "$file")
  mkdir -p "$dir" || return 1
  tmp=$(mktemp "$dir/.autoship-pin.XXXXXX") || return 1
  if printf '%s\n' "$value" > "$tmp" && mv -f "$tmp" "$file"; then return 0; fi
  rm -f "$tmp"
  return 1
}

# Export PIN from the deploy checkout's object store and build it in its own
# target directory on the build volume, through the updater's normal storage
# admission (run_update_build). The source tree is not a Git worktree, so
# autoship's own checkout bookkeeping never sees it.
autoship_pin_build() {
  local pin="$1" root src status=0
  root=$(autoship_pin_root); src="$root/src"
  if ! git -C "$REPO_ROOT" cat-file -e "$pin^{commit}" 2>/dev/null; then
    git -C "$REPO_ROOT" fetch origin main --quiet >/dev/null 2>&1 || true
  fi
  if ! git -C "$REPO_ROOT" cat-file -e "$pin^{commit}" 2>/dev/null; then
    log "AUTOSHIP PIN BUILD FAILED: commit $pin is not in $REPO_ROOT; the worker keeps its current binary"
    return 1
  fi
  if [ -z "${AUGMENTAGENT_REMOTE_BUILD_HOST:-}" ] && declare -F memory_pressure_ok >/dev/null && ! memory_pressure_ok; then
    log "pinned autoship build deferred under memory pressure; retry next tick"
    return 1
  fi
  mkdir -p "$root" || return 1
  rm -rf "$src.new" && mkdir -p "$src.new" || return 1
  if ! git -C "$REPO_ROOT" archive --format=tar "$pin" | tar -x -C "$src.new"; then
    rm -rf "$src.new"
    log "AUTOSHIP PIN BUILD FAILED: could not export $pin; the worker keeps its current binary"
    return 1
  fi
  rm -rf "$src" && mv "$src.new" "$src" || return 1
  log "building pinned autoship worker $pin (source $src, target $root/target)"
  (
    cd "$src" || exit 1
    export CARGO_TARGET_DIR="$root/target" AUGMENTAGENT_PHONE_BUILD_COMMIT="$pin"
    run_update_build "$DAEMON_BINARY" "$root/target" "$pin"
  ) || status=$?
  if [ "$status" -ne 0 ]; then
    log "AUTOSHIP PIN BUILD FAILED or deferred (status $status) for $pin; the worker keeps its current binary; retry next tick"
    return 1
  fi
}

# Put PIN's executable in bin/ (atomic rename, so the running worker sees a
# new inode) and then record it in the stamp the worker watches. PREBUILT is
# the commit the deploy checkout's target/release was just built from; when
# it is the pin, that binary is copied instead of compiling a second time.
autoship_pin_install() {
  local pin="$1" prebuilt="$2" root source
  root=$(autoship_pin_root)
  if [ -n "$prebuilt" ] && [ "$pin" = "$prebuilt" ] && [ -x "$REPO_ROOT/target/release/augmentagent" ]; then
    source="$REPO_ROOT/target/release"
    log "installing pinned autoship worker $pin from the deploy build (no second compile)"
  else
    autoship_pin_build "$pin" || return 1
    source="$root/target/release"
  fi
  if [ ! -x "$source/augmentagent" ]; then
    log "AUTOSHIP PIN INSTALL FAILED: $source/augmentagent missing; the worker keeps its current binary"
    return 1
  fi
  mkdir -p "$root/bin" || return 1
  if ! { cp "$source/augmentagent" "$root/bin/.augmentagent.new" &&
         chmod 755 "$root/bin/.augmentagent.new" &&
         mv -f "$root/bin/.augmentagent.new" "$root/bin/augmentagent"; }; then
    rm -f "$root/bin/.augmentagent.new"
    log "AUTOSHIP PIN INSTALL FAILED: could not install $root/bin/augmentagent"
    return 1
  fi
  autoship_pin_write "$(autoship_pin_stamp)" "$pin" || return 1
  log "AUTOSHIP PIN INSTALLED: worker binary is now built from $pin; the worker reloads at its next safe point"
}

# The updater calls this before worker provisioning, every tick. Sets
# AUTOSHIP_PIN_CHANGED=1 when the worker's binary or mode changed, so the
# caller re-provisions the unit. BOOTSTRAP is the commit this tick's deploy
# build came from: with no pin recorded yet, the worker is pinned to exactly
# the build it would have reloaded onto anyway, so the rollout changes nothing.
# shellcheck disable=SC2034  # read by the sourcing updater
AUTOSHIP_PIN_CHANGED=0
reconcile_autoship_pin() {
  local bootstrap="${1:-}" prebuilt="${2:-}" pin installed present=0 decision
  AUTOSHIP_PIN_CHANGED=0
  autoship_pin_enabled || return 0
  if ! pin=$(autoship_pin_read); then
    log "AUTOSHIP PIN INVALID: $(autoship_pin_file) must hold a full commit SHA or 'main'; worker binary left unchanged"
    return 1
  fi
  installed=$(autoship_pin_installed)
  [ -x "$(autoship_pin_binary)" ] && present=1
  decision=$(autoship_pin_decision "$pin" "$installed" "$present" "$bootstrap")
  case "$decision" in
    none|unchanged) return 0 ;;
    unpin)
      log "autoship pin is 'main': returning the worker to the main deploy build"
      AUTOSHIP_PIN_CHANGED=1
      return 0 ;;
    bootstrap)
      autoship_pin_write "$(autoship_pin_file)" "$bootstrap" || return 1
      log "AUTOSHIP PIN BOOTSTRAP: no pin recorded; pinned the worker to $bootstrap, the build this deploy installs. Move it with scripts/autoship-pin.sh <sha|tag>"
      pin="$bootstrap"
      if [ "$installed" = "$pin" ] && [ "$present" = 1 ]; then return 0; fi ;;
  esac
  autoship_pin_install "$pin" "$prebuilt" || return 1
  AUTOSHIP_PIN_CHANGED=1
}

# After provisioning has put the unit back on scripts/run-rs.sh, record the
# unpin in the stamp the pinned worker watches, so it reloads under that unit.
autoship_pin_after_provision() {
  autoship_pin_enabled || return 0
  [ "$(autoship_pin_read 2>/dev/null)" = main ] || return 0
  [[ "$(autoship_pin_installed)" =~ ^[0-9a-f]{40}$ ]] || return 0
  autoship_pin_write "$(autoship_pin_stamp)" main || return 1
  log "AUTOSHIP UNPINNED: the worker reloads onto the main deploy build at its next safe point"
}
