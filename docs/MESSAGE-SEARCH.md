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

## Measuring retrieval (and the embeddings decision)

`augmentagent messages eval` scores retrieval against your own labelled
questions. It answers one question: after structured search, how often does
the agent still fail to find the right messages, and are those failures the
kind semantic search would fix?

```sh
augmentagent messages eval-template > ~/private/eval-questions.json   # schema
augmentagent messages search 'restaurant after:2026-06-01'            # find ids to label
augmentagent messages eval --questions ~/private/eval-questions.json --k 10
augmentagent --wiki-dir ./wiki messages eval --questions ~/private/eval-questions.json --agent
```

- **Question sets are private.** They hold your message ids, so the file must
  live outside this repo; the command refuses a path inside it. Only the
  schema and the scoring ship here.
- **Two modes.** Without `--agent` each question's `query` runs against the
  tools directly (fast, no model, what CI exercises). With `--agent` the real
  ask agent picks its own tool calls — one model call per question.
- **Reports** carry ids, metrics and rubric labels, never message text, so
  they are safe to paste into an issue.
- **Miss classes:** `vocabulary` (the message shares no content word with the
  question — the class embeddings would address), `agent` (a query over the
  question's own words would have found it), `structure` (words match but no
  operator expresses the constraint), `data` (not in the store).

**Decision rule, fixed before measuring:** vocabulary misses under 10% of
questions → no embeddings. 10–25% → try cheaper fixes first (alias expansion
from the wiki, agent query rewriting) and re-measure. Over 25% → open an
embeddings issue scoped to the classes that missed, with a pluggable provider
(local by default; a hosted provider sends message text to a third party),
windowed chunks rather than single messages, and hybrid ranking with FTS.
The command prints the verdict for the report it just produced.
