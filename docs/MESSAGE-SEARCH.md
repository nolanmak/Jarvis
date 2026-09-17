# Structured message search

Every stored message — texts, chat apps, DMs, server channels, notes, email —
is normalized into a `message_index` row and a full-text entry, so the agent
can answer questions about a person, a time window, a platform or an exact
phrase, and can count things ("who do I message most") without paging.

Nothing here calls a model. Indexing, backfill, people resolution, search and
stats are database operations.

## Tools the agent has

- `search_messages(query, limit?, offset?)` — operator search (below).
- `conversation_stats(group_by, …)` — counts, rankings, first/last contact.
  Returns counts and timestamps only, never message text.

Both are read-only and are also available on the CLI for humans:

```sh
augmentagent messages search 'with:alex from:me is:latest'
augmentagent messages stats --group-by person --kind dm --limit 10
```

## Operators

| operator | meaning |
|---|---|
| `with:<person\|handle>` | conversations that person takes part in, including your own messages there. Repeatable. |
| `from:<person\|me>` / `to:<person>` | sender / messages you sent them |
| `in:<platform[,platform]>` | `imessage`, `whatsapp`, `discord`, `gmail`, `apple_notes`, `linkedin`, … |
| `is:dm` `is:group` `is:channel` `is:note` `is:email` `is:meeting` | conversation kind |
| `is:latest` | newest single match |
| `server:<name>` `channel:<name>` | server / channel name; quote when it has spaces |
| `thread:<conversation_id>` | one conversation |
| `after:` `before:` `on:` | `YYYY-MM-DD`, ISO-8601, or `7d` / `3w` / `6m` / `1y` |
| `has:attachment` `has:link` | flags |
| `sort:relevance\|newest\|oldest` | default: relevance with text, else newest |
| `-<operator>:<value>`, `-word` | exclude |
| bare words, `"exact phrase"`, `prefix*` | full-text, stemmed and ranked |

A person reference that matches several people returns no rows and an
`ambiguous` list — the tool never picks for you. A reference no person page
claims is used as a raw handle and reported in `unresolved`.

## How a person spans platforms

Handles (phone numbers, WhatsApp JIDs, Discord ids, email addresses) map to
person pages through the wiki's `identities:` front matter, cached in
`message_people`. Rebuild after editing pages:

```sh
augmentagent --wiki-dir ./wiki messages resolve-people
```

The daemon refreshes it every 15 minutes. A handle claimed by several *named*
pages maps to nobody (reported as a conflict) rather than being guessed; a
handle claimed by one named page plus id-only stub pages maps to the named
page.

## Index maintenance

Triggers on `emails` queue every insert, content change and delete; the daemon
drains the queue every 15 seconds. Backfill and health:

```sh
augmentagent messages reindex          # resumable; no-op when complete
augmentagent messages reindex --dry-run
augmentagent messages check            # exits non-zero when incomplete
```

`augmentagent doctor` reports index coverage as an ok/warn finding. Run `messages reindex` **after** deploying a
build that changes extraction: the daemon drains the same queue, so a reindex
run against an older daemon leaves rows stale (and `check` says so).

Backfill works in small batches with a pause between them so other writers
(daemon, dashboard) are never starved of the write lock.
