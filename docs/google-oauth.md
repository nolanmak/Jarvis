# Connecting Gmail through your own Google OAuth client

By default the dashboard connects Gmail through Composio, which hosts a Google
OAuth client, stores each mailbox's refresh token and proxies every Gmail call
(metered on its plan). With your own OAuth client the agent talks to the Gmail
REST API directly: same one-click consent per mailbox, zero Composio calls for
those mailboxes.

## One-time setup in Google Cloud (about 15 minutes)

1. Create a project (console.cloud.google.com → project picker → New Project).
2. **APIs & Services → Library**: enable *Gmail API*, *Google Calendar API*,
   *Google Drive API* and *People API*. One client covers all four.
3. **OAuth consent screen** (Google Auth Platform → Branding): app name,
   support email, developer contact. User type **External**.
4. **Audience**: publishing status **In production**. An app left in *Testing*
   expires every refresh token after 7 days, which disconnects every mailbox
   weekly. Unverified-but-production is fine for up to 100 accounts; each
   account sees a "Google hasn't verified this app" page once
   (Advanced → Continue).
5. **Data Access → Add scopes**:
   - `https://www.googleapis.com/auth/gmail.modify`
   - `https://www.googleapis.com/auth/calendar`
   - `https://www.googleapis.com/auth/drive`
   - `https://www.googleapis.com/auth/contacts.readonly`
6. **Clients → Create client**: type **Web application**. Authorized redirect
   URI, exactly: `http://localhost:<DASHBOARD_PORT>/oauth/google/callback`
   (port 3000 unless you changed `DASHBOARD_PORT`). Google allows plain http
   for localhost. If the dashboard is served on a real hostname, register that
   URL too and open the dashboard through it when connecting.
7. Copy the client id and secret into `.env`:

   ```
   GOOGLE_OAUTH_CLIENT_ID=...
   GOOGLE_OAUTH_CLIENT_SECRET=...
   ```

Restart the dashboard. Settings → **+ Add Gmail** now sends the browser to
Google's account chooser; after consent the mailbox appears with a *Google*
badge and is polled directly. Google Workspace domains may require an admin
to allow the app once (Admin console → Security → API controls).

## What happens under the hood

- `GET /oauth/google/start` stores a one-time CSRF state and redirects to
  Google with `access_type=offline` and `prompt=consent` (so a refresh token is
  issued on every connect).
- `GET /oauth/google/callback` verifies the state and runs
  `augmentagent google connect`, which exchanges the code, reads
  `users.getProfile` to learn the address, stores the refresh token in the
  private credential store (`augmentagent/google/<address>`) and registers the
  mailbox as entity `google:<address>`. A Composio row for the same address is
  deactivated so one transport polls the inbox.
- The daemon's Gmail client routes `google:` entities to the Gmail REST API
  (search, threads, drafts, send, attachments) and everything else to Composio,
  so the two can coexist while mailboxes are reconnected one by one.
- Access tokens are refreshed a minute before expiry. An `invalid_grant`
  answer (access revoked, or a Testing-status token that aged out) is latched
  and the mailbox reports "reconnect it from the dashboard" instead of
  retrying every tick.
- **Remove** on a Google mailbox revokes the grant at Google and deletes the
  stored token.

## Connecting without the Settings page

The same attempt flow is reachable from the terminal and from the agent:

- **CLI:** `augmentagent google connect-start` asks the running dashboard for
  a consent URL, prints it (and opens it when a display is available), then
  waits until the browser callback has saved the mailbox.
- **Discord / chat:** ask the agent to connect a Gmail account. It calls the
  owner-only auth tool, which returns the Google consent URL; open it, approve,
  and the mailbox is saved when the callback lands. Asking the agent to check
  reports connected once it has.

Either way the consent finishes by Google redirecting the browser to the
dashboard's callback. Out of the box that is `http://localhost:<port>/…`, so
the approve click has to happen on the dashboard host. To approve from a
phone or another computer, give the dashboard an https origin those devices
can reach and tell both sides about it:

1. Expose the dashboard, for example with Tailscale Serve on the host:
   `tailscale serve --bg --https=443 http://127.0.0.1:<DASHBOARD_PORT>`
   (your tailnet's HTTPS certificates must be enabled). Any reverse proxy
   with a valid certificate works the same way.
2. Add `https://<that hostname>/oauth/google/callback` as a second authorized
   redirect URI on the OAuth client in Google Cloud. Google rejects any URI
   that is not registered byte for byte.
3. Set `AUGMENTAGENT_PUBLIC_URL=https://<that hostname>` in `.env` and
   restart the dashboard. New consents now redirect there; the callback is
   reachable without a dashboard login, and the saved mailbox shows up for
   the CLI, the agent and Settings alike.

## CLI

```
augmentagent google connect-start       # start a connect from the terminal
augmentagent google accounts            # Google-connected mailboxes + grant state
augmentagent google check --account me@example.com
augmentagent google disconnect --account me@example.com
```

Calendar, Drive and Contacts still go through Composio for now; the token
stored here already carries their scopes so those ports need no new consent.
Calendar polling skips Google-connected mailboxes until that port lands.
