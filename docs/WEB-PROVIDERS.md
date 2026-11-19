# Web providers

Models that Jarvis runs through the Codex bridge but that are not OpenAI
models (Qwen, GLM, and the yolo profile) cannot run the native web tools.
For those turns Jarvis attaches its own `mcp__web__search` and
`mcp__web__fetch`, backed by the vendors below. Claude and Codex keep their
native web tools and are not affected.

Tracking: epic #2200, spec #2242.

## Providers and credentials

A provider is usable for search or for fetch only when every credential in
its row is set **for the daemon**: in the credential store or the daemon's
environment. The fetch sidecar (`sidecars/fetch/`) is a separate component
that reads its own `fetch.env`; keys set only there are not seen here, so set
them in both places if you use both.

| Id | Search | Fetch | Credentials | Unfiltered results (yolo turns) |
|---|---|---|---|---|
| `brave` | yes | no | `BRAVE_API_KEY` | Jarvis sends `safesearch=off` |
| `exa` | yes | yes | `EXA_API_KEY` | unfiltered by default |
| `firecrawl` | yes | yes | `FIRECRAWL_API_KEY` | unfiltered by default |
| `context` | yes | yes | `CONTEXT_DEV_API_KEY` | no switch; the vendor may filter |
| `serpapi` | yes | no | `SERPAPI_KEY` (or `SERPAPI_API_KEY`) | Jarvis sends `safe=off` |
| `brightdata` | yes | yes | search: `BRIGHTDATA_API_KEY` + `BRIGHTDATA_SERP_ZONE`; fetch: `BRIGHTDATA_API_KEY` + `BRIGHTDATA_UNLOCKER_ZONE` | not requested yet |

Outside a yolo turn the vendor's own safe-search default is left alone.

Providers whose client has not landed yet are listed by `/search list` as
`missing not implemented`.

`augmentagent doctor` reports what is usable in the `web_providers` finding,
and warns when Qwen or GLM is enabled and nothing can search.

## Which provider serves a call

Search and fetch are resolved separately, because some providers cannot
fetch. For one call:

1. the conversation's pin, if that provider is usable for the kind;
2. else the default pin, if usable;
3. else the first usable provider in the preference order.

Preference order for search: `brave`, `exa`, `firecrawl`, `context`,
`serpapi`, `brightdata`. For fetch: `firecrawl`, `exa`, `context`,
`brightdata`.

A pin turns failover off: if the pinned provider fails, the tool reports the
failure and names the other usable providers. With no pin, a provider fault
(timeout, connection failure, any status other than 400 or 422, a malformed
reply) moves on to the next provider; a 400 or 422 is returned at once,
because another vendor would reject the same request. Each provider gets one
attempt (10 s for search, 45 s for fetch, 90 s for Bright Data fetch) inside a
100 s budget for the whole call. A stored pin that is not usable is skipped
and named in the tool's result.

One turn can make at most 30 search and fetch calls.

## The `/search` command

Owner only, in a DM or the query conversation. Only the slash form is a
command; a message that starts with the word `search` is an ordinary
question. It never calls a vendor.

| Command | Effect |
|---|---|
| `/search` or `/search status` | The provider in effect for search and for fetch in this conversation, where it comes from, any stored pin that is unusable, and what is available. |
| `/search list` | Every provider, and for each kind whether it is available or which credential names are missing. Values are never shown. |
| `/search provider <id>` | Pin search. Also pins fetch when that provider can fetch right now; otherwise the fetch pin is left alone and the reply says why. |
| `/search fetch <id>` | Pin fetch only. |
| `/search provider auto` or `/search reset` | Clear both pins. |
| `/search fetch auto` | Clear the fetch pin. |

Add `scope:default` as the last word to change the default instead of this
conversation. In a conversation, `auto` means "use the default"; the reply
shows the provider now in effect.

The agent can pin a provider for the current conversation with
`mcp__web__set_provider` when the owner asks. It cannot change the default
or another conversation, and yolo turns do not have this tool.

Pins live in `~/.config/augmentagent/web-providers.json`
(`AUGMENTAGENT_WEB_PROVIDERS_CONFIG` overrides the path).

## What leaves the machine

Every search query and every fetched URL is sent to the provider that serves
the call. Fetches always go through a vendor: Jarvis never requests the
target page itself, and it refuses URLs that point at local or private
hosts before contacting any vendor. Page and search output is marked as
untrusted data for the model.
