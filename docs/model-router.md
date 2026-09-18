# Model accounts through 9Router

The Rust agent's existing Claude and Codex CLI adapters can route inference through
a local 9Router service. Their tool permissions, constrained Codex MCP bridge,
approval hooks, handoff journals, and provider attribution remain in the agent.
9Router owns account login, token refresh, priority, and account cooldowns.

## Install and connect

On Linux, as the user who runs the agent:

```sh
python3 scripts/install-model-router.py
```

The installer requires Node.js 22+, npm, Git and systemd user services. It pins
9Router to `17c4cc76877bd1755030a8414f8d0083f48dcccf` (0.5.75), installs dependencies
using the committed lockfile, builds its standalone server, and starts
`augmentagent-model-router.service` on **127.0.0.1:20128**. Install both the Claude
and Codex CLIs for the corresponding routes. Re-running the installer preserves
accounts, API keys and the selected route. `--built-source PATH` can reuse an
already built checkout at the pinned commit during local development. The
installer applies `sidecars/9router/runpod-reconciliation.patch` before building;
a supplied built source must already contain it. The service uses a distinct
runtime directory for this patched version. For Docker, run
`bash scripts/build-model-router-image.sh` to build
`jarvis-9router:0.5.75-runpod-3` from the same source and dependency lock.

After deploying this agent version, open **Settings → Models & accounts**:

1. Choose **Connect Claude** or **Connect Codex**, then open the sign-in link.
2. Sign in with the account you want. For another account, use a private browser
   window or switch accounts on the provider's sign-in screen.
3. Paste the returned authorization code (Claude, including `#state`) or complete
   callback address (Codex) into the connection form. Codex's localhost callback
   may show a connection error when the browser is on another machine; its URL
   still contains the code needed here. No callback port needs to be exposed.
4. Repeat for every account. Lower priority numbers are tried first. Disable other
   accounts for that provider if you want to use one specific account only.
   Enable the replacement first: the dashboard rejects disabling the last active
   account required by the current route. Switch to direct mode to disable them all.
5. Select **Claude accounts only**, **Codex accounts only**, or **Auto — Claude,
   then Codex**, and save. Auto requires an enabled account for both providers.

The initial route is **Existing CLI accounts (9Router off)**. A route change takes
effect on the next call; each in-flight call keeps one configuration snapshot
through its fallback attempts. After the first installation, restart/deploy the
agent once so it registers both gateway adapters; subsequent account, route, and
model changes do not require a restart. Direct mode uses the original
`AUGMENTAGENT_REASONER_CHAIN`; gateway Auto is explicitly Claude then Codex.

Quality and fast model IDs must be available to your accounts. The defaults
match this agent's existing defaults; availability still needs a live provider
smoke test after sign-in. Claude routes accept only `cc/…` models and Codex routes
only `cx/…`. Cross-provider 9Router combos are intentionally not accepted: the
agent's independent-review provenance must identify the actual model provider.

## Storage and boundaries

Configuration is stored at
`$XDG_CONFIG_HOME/augmentagent/model-router.json` (default
`~/.config/augmentagent/model-router.json`). Override with
`AUGMENTAGENT_MODEL_ROUTER_CONFIG` in both the daemon and dashboard environment.
The dashboard atomically writes mode-0600 files. Public responses and templates
omit API keys, dashboard passwords and provider tokens. Codex receives its gateway
key through a dedicated environment variable, never a command-line argument.
The upstream dashboard password and JWT secret are in mode-0600 `9router.env`.
Account tokens live in 9Router's private data directory under
`$XDG_DATA_HOME/augmentagent/9router/data`.

Our dashboard's existing login and Host/Origin checks protect all account routes.
OAuth verifiers are held server-side for ten minutes, keyed by a random session
ID. Each exchange validates provider, callback and state and consumes its session
once. Restarting the dashboard requires restarting unfinished login flows. New flows
resolve the current router configuration; pending flows retain their original
gateway client so changing endpoints does not send an exchange to another server.

The installer disables cloud sync, tunnels, prompt rewriting, compression and
cross-provider capacity adapters. The CLI requests also set the token-saver
bypass header. Native CLI quota latches are kept separate: 9Router manages the
connected account pool's availability. Routing does not replay completed tool
operations or change the existing failover policy for content/local failures.

## Verify and roll back

```sh
systemctl --user status augmentagent-model-router.service
npm test
cargo test -p augmentagent-channel-core --lib
python3 -m unittest discover -s scripts/tests -p '*_test.py'
```

Opt-in integration checks use synthetic content only:

```sh
# Real router, two temporary local API accounts: first returns quota, second succeeds.
# Temporary provider and both connections are deleted in finally.
JARVIS_TEST_MODEL_ROUTER_CONFIG="$HOME/.config/augmentagent/model-router.json" \
  python3 scripts/tests/model_router_live_test.py -v

# Real installed Claude/Codex CLIs and the built agent, with a local fake gateway.
JARVIS_TEST_ROUTER_AGENT_BIN="$PWD/target/release/augmentagent" \
  python3 scripts/tests/model_router_live_test.py -v

# After the owner has signed into real subscription accounts and selected a route:
./target/release/augmentagent reasoner-selftest --prompt 'Reply exactly ROUTER_OK'
```

The synthetic checks prove transport, streaming, auth forwarding, account failover
and disabling accounts. They do **not** prove that a particular subscription can
access a model or complete a real provider login. Verify those after connecting
each account. The existing provider output-contract suites are opt-in live
fixtures for structured output and wiki/tool operations.

For a Docker-hosted router, add `"upstream_host":"host.docker.internal"` to a
private copy of the router test config before running the synthetic suite. The
patched router must pass quota failover, request-key/409 and unsupported-image
tests. Stock 0.5.75 fails the latter two. Keep the original data volume backed up
when switching an existing router to the patched image.

The patched build treats a direct OpenAI-compatible model selection as pinned:
9Router will not switch it to a capacity-adapter provider. When the selected
model's declared capabilities cannot read a current-turn image, document,
audio or video input, it returns 422 before contacting an upstream. This
prevents a normal-looking answer after an attachment was removed. A capability
declaration is not proof that the deployed model handles that input; the live
attachment acceptance test remains required before enabling the profile.

To roll back, select **Existing CLI accounts (9Router off)**. Then optionally stop
`augmentagent-model-router.service`; stored accounts are retained. If the router
is unavailable, the dashboard still lets you save the direct route. Malformed
configuration fails closed rather than silently sending requests via another
account.

## Sources

- [9Router source at the pinned revision](https://github.com/decolua/9router/tree/17c4cc76877bd1755030a8414f8d0083f48dcccf)
- [Codex custom model provider configuration](https://learn.chatgpt.com/docs/config-file/config-reference)
