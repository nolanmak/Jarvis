#!/bin/bash
# Socket Firewall in front of npm, npx and pnpm. `install-tools.sh sfw` copies
# this into its shim directory, symlinks those three names to it, and puts that
# directory first on PATH for the rest of the job. Run it after
# actions/setup-node, which prepends its own npm to PATH.
#
# sfw runs a command behind a local proxy and blocks packages Socket has flagged
# as malware. Everything the command starts inherits the proxy (HTTP_PROXY,
# SSL_CERT_FILE, NODE_EXTRA_CA_CERTS), so `npm test` and `npm run ...` are passed
# straight through: they fetch no packages and tests must not run behind it.
# Anything not on that short list is wrapped, so a subcommand or option this
# script cannot parse is covered rather than missed.
#
# Limits, in the README under "Socket Firewall": with Socket unreachable sfw
# installs nothing and fails; and sfw only sees what reaches the network, which
# is why install-tools.sh sets npm_config_prefer_online.
set -u
name=${0##*/}
here=$(cd "$(dirname "$0")" && pwd)

# Off PATH, so the real tool is found and sfw never loops back through here.
clean=:$PATH:
clean=${clean//:$here:/:}
clean=${clean#:}
export PATH=${clean%:}

command -v "$name" >/dev/null || { echo "$name: not installed on this runner" >&2; exit 127; }

# Already behind sfw: an explicit `sfw npm ci`, or a lifecycle script calling npm.
case "${NODE_EXTRA_CA_CERTS:-}" in
  *socketFirewallCa*) exec "$name" "$@" ;;
esac

# npx can always fetch. For npm and pnpm, look at the first non-option argument.
if [ "$name" != npx ]; then
  sub=
  for arg in "$@"; do
    case "$arg" in
      -*) ;;
      *) sub=$arg; break ;;
    esac
  done
  case "$sub" in
    ""|run|run-script|rum|urn|test|tst|t|start|stop|restart|version|help|config|c|ls|list|ll|la|prefix|root|bin)
      exec "$name" "$@" ;;
  esac
fi
exec sfw "$name" "$@"
