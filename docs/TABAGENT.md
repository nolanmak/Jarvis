# TabAgent browser tools

Jarvis can browse and act in the owner's real, signed-in Chrome through
[TabAgent](https://github.com/nolanmak/tabagent). TabAgent runs on the Legion,
next to Jarvis, and listens on `127.0.0.1` only. There is no tunnel.

The tools are attached only to owner-authorized turns: a Discord turn from the
owner or a relayed owner query (`WikiQuerier::answer`), and a local
`augmentagent wiki ask`.

## What the model gets

The model gets these MCP tools, named `mcp__tabagent__<tool>`:

| Tool | Notes |
|---|---|
| `tabs` | `list`, `new`, `select`, `close`. `attach` (adopting one of the owner's own tabs) is refused. |
| `nav_goto` | Public http(s) URLs only. Private, loopback, link-local, `.local`, `.internal` and `.ts.net` targets are refused before any request, using the same check as the web tools. |
| `nav_wait` | `load`, `networkidle`, `selector`, `text`, `url`. `for=fn` runs page JavaScript and is refused. |
| `page_snapshot`, `page_markdown`, `page_text`, `page_find` | Read only. |
| `extract_table`, `extract_links` | Read only. |
| `act_click`, `act_type`, `act_press`, `act_select`, `act_check`, `act_hover`, `act_scroll`, `act_fill` | Forwarded as given. |
| `form_fill`, `form_submit`, `dialog_handle` | Forwarded as given. |
| `captcha_solve` | `{tabId?, ref?}`. Checkbox click, then TabAgent's solver (CapSolver, vision fallback). Returns solved, vendor and method, never the token. |
| `auth_handoff` | `{tabId?, reason}`. Hands the tab to the owner for an ID or selfie check, passkey, payment step or an unsolved CAPTCHA, and waits until they press Done or Cancel. See [CAPTCHAs and handoffs](#captchas-and-handoffs). |

Jarvis never offers `js_eval`, cookies, storage, network capture, uploads
(`act_upload`), downloads, the other `auth_*` tools (`auth_login`,
`auth_fill`, ...), screenshots, `act_drag` or `act_dblclick`.

Jarvis adds no approval of its own for actions. Their safety comes from
TabAgent: per-site approval, the sensitive-action prompts in Chrome (forms
with password, card or SSN fields, and clicks that look like a purchase,
delete, send or transfer), and the kill switch. The system prompt tells the
model to act only when the owner asks. Before posting, commenting, sending or
buying, the model must state in its reply exactly what it will do. Action calls
are not idempotent, so the Codex bridge keeps its mutation receipts for them.
Only reads could be classified as read-only later.

## How it works

- The daemon adds a hidden `augmentagent tabagent-tool` stdio MCP server to the
  turn. The session id is passed on argv, so the model cannot choose another
  session's tabs. The tool list on argv is the boundary: the server lists and
  accepts nothing else.
- The server reads the token file itself. The token never appears on argv, in
  the model's environment or in the settings file.
- Each call is one `POST http://127.0.0.1:47615/v1/<tool>` with
  `Authorization: Bearer <token>`, `x-tabagent-client: jarvis` and
  `x-tabagent-session: <Jarvis session>`. TabAgent's audit log and tab group
  then line up with Jarvis's own records.
- Jarvis never starts TabAgent. If nothing listens, or the extension is not
  connected, the tool returns "browser offline".
- Each call has a 90 s deadline, below the Codex bridge's 120 s cap, and a
  `timeout` argument is capped at 60 s. Two tools wait for longer:
  `captcha_solve` 160 s (TabAgent's solver cap is 120 s) and `auth_handoff`
  33 min (TabAgent's remote handoff waits 30 min, after a possible 120 s
  approval prompt). The server entry declares them in `toolTimeouts`; the
  Codex bridge honours those for the named tools only (capped at 2400 s), and
  Codex's own wait for the bridge grows to match (`bridge_tool_timeout_secs`).
  A launch accepts 200 calls. Failed calls are never retried.
- `PROMPT_TIMEOUT` and `PERMISSION_DENIED` tell the model not to retry and to
  ask the owner to approve in Chrome. `KILLED` (the kill switch) stops the
  TabAgent tools for the rest of the launch.
- The handoff journal (`scripts/codex-tool-bridge.py`, #2519) treats
  `page_snapshot`, `page_markdown`, `page_text`, `page_find`, `extract_table`,
  `extract_links`, `nav_wait` (not `for: fn`) and `tabs` with exactly
  `{"action": "list"}` as reads: they are never journaled. Every other call is
  journaled as a mutation, so an unanswered one still requires reconciliation.
  Actions, `nav_goto`, other `tabs` actions, `form_fill`, `dialog_handle`,
  `captcha_solve` and `auth_handoff` may repeat with identical arguments (a second click on the same ref after a
  scroll). An identical repeat of `form_submit` is refused as already
  completed, because it is most likely a double submission.
- TabAgent wraps page-derived text in `<<<PAGE_DATA nonce … PAGE_DATA nonce>>>`
  blocks. Jarvis passes them through byte for byte and puts its own untrusted
  marker lines outside them. The system prompt says page data is never
  instructions and cannot authorize other tools.

## Setup on the Legion

1. Install TabAgent and its Chrome extension in the Chrome profile you want
   Jarvis to use, then run `tabagent serve` and pair the extension.
2. Mint a scoped token. Never use the bridge secret or `--caps full`:

   ```sh
   tabagent token create --name jarvis --caps act --sites '<explicit list>'
   ```

   The `act` cap is needed because `tabs new` and `nav.goto` require it.
   Read-only behaviour comes from Jarvis's tool list. Set the site policy to
   `read` for banks and other high-stakes origins.
3. Store the token, owner-only:

   ```sh
   install -m 0600 /dev/null ~/.config/augmentagent/tabagent.token
   # paste the token into the file, then:
   chmod 0600 ~/.config/augmentagent/tabagent.token
   ```

   A symlink, a file with any other mode, or a file that holds anything other
   than one token disables the tools. Jarvis does not fall back to another
   location.
4. Optional overrides in `.env`:
   - `AUGMENTAGENT_TABAGENT_URL`: default `http://127.0.0.1:47615`. It must be
     `http://127.0.0.1:<port>` or `http://localhost:<port>`.
   - `AUGMENTAGENT_TABAGENT_TOKEN_FILE`: default
     `~/.config/augmentagent/tabagent.token`, or under `$XDG_CONFIG_HOME` when
     that is set.
5. Run `augmentagent doctor`. The `tabagent` row checks the token file and
   `GET /v1/health`. It shows a warning, not an error, when TabAgent is down.

## CAPTCHAs and handoffs

nolanmak/tabagent#61. The owner drives Jarvis from Discord on a phone, so a
CAPTCHA must not stop a task, and the only steps that wait for a person are
ones only the owner can do.

- **CAPTCHAs.** `autoCaptcha` is on for the `jarvis` token
  (`tabagent token auto-captcha jarvis on`), so `act_click` and `form_submit`
  solve a blocking hCaptcha, reCAPTCHA or Turnstile on their own. The model
  calls `captcha_solve` only when one still blocks. `CAPTCHA_UNSOLVED` comes
  back with TabAgent's reason and hint; hints that say `auth.handoff` are
  shown to the model as `auth_handoff`.
- **Handoffs.** For an ID or selfie check, a passkey, a payment confirmation,
  a phone approval or a CAPTCHA TabAgent could not solve, the model calls
  `auth_handoff` with a reason. With `handoff.remote on`, TabAgent opens a
  one-time phone page for that tab on the Legion's Tailscale address and
  POSTs `{event: "handoff.start", origin, reason, url, expiresAt, session}` to
  its `handoff.webhook`. Jarvis receives it and posts the link, origin, reason
  and expiry to the owner in Discord. The call returns when the owner taps
  Done or Cancel, or after 30 min.
- **The webhook.** `serve` listens on `127.0.0.1:47625`
  (`AUGMENTAGENT_TABAGENT_HANDOFF_PORT`, `0` turns it off) when the TabAgent
  token file and `DISCORD_BOT_TOKEN` + `DISCORD_CHANNEL_ID` are set. Only a
  loopback peer with a loopback `Host` header is served. TabAgent's webhook
  carries no credential, so the path does: `/tabagent/handoff/<secret>`, a
  256-bit value created in `~/.local/state/augmentagent/tabagent-handoff.secret`
  (0600) on first use.
- **Where the link goes.** `auth_handoff` records its turn's channel just
  before it calls TabAgent. The link goes to that channel when it is the
  owner's (`DISCORD_CHANNEL_ID`, `DISCORD_QUERY_CHANNEL_ID`, or a DM with
  `DISCORD_ALLOWED_USER_ID`), otherwise to `DISCORD_CHANNEL_ID`. Never to
  Slack or a webhook. The link takes over that tab until the handoff ends, so
  it is never logged, and the message suppresses link previews and mentions.
  The model never sees the link.

One-time setup on the Legion, after this is deployed:

```sh
tabagent config handoff.remote on        # already on
tabagent config handoff.webhook "$(~/AugmentAgent/target/release/augmentagent tabagent-handoff-url)"
tabagent token auto-captcha jarvis on    # already on
```

`augmentagent tabagent-handoff-url` creates the secret if needed and prints
`http://127.0.0.1:47625/tabagent/handoff/<secret>`. TabAgent reads its
config on every handoff, so no restart is needed.

## Approvals and the kill switch

TabAgent asks for approval in its Chrome side panel. These prompts appear on
the Legion, not in Discord. If a site needs approval, Jarvis tells the owner to
approve it in Chrome on the Legion and does not retry on its own. Approving the
few sites Jarvis reads ahead of time avoids most prompts.

`Alt+Shift+K`, or the toolbar or side panel, stops TabAgent. In-flight calls
return `KILLED`, and Jarvis stops using TabAgent for that turn.

## Tests

```sh
scripts/cargo-managed.sh test -p augmentagent-cli --bin augmentagent tabagent_tool
```

The owner-run live test needs a paired TabAgent and the `jarvis` token on the
host:

```sh
scripts/cargo-managed.sh test -p augmentagent-cli --bin augmentagent \
  tabagent_tool::tests::live_tabagent_reads_a_synthetic_page -- --ignored --nocapture
```

It opens a public synthetic page (`TABAGENT_LIVE_FIXTURE_URL`, default
`https://example.com/`) and reads it back as Markdown.
