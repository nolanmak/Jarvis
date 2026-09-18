# Embeddings (semantic layer)

Optional. With nothing configured, the daemon downloads no model, writes no
vectors and makes no new network calls. Everything here is off until you turn
it on.

## What it is

Messages are grouped into conversation windows ("chunks": consecutive
messages in one conversation, split on a 30-minute gap or at 40 messages /
4,000 characters; email, notes and meetings are one chunk each). Each chunk
gets a vector from a small sentence-embedding model, so retrieval can match
by meaning as well as by keyword. Vectors are derived data: rebuildable at
any time, and every row records the provider, model and dimension that
produced it, so two models' vectors are never compared.

## Local model (default)

The default provider runs `bge-small-en-v1.5` (MIT) through ONNX Runtime on
the CPU. No key, no network at runtime, message text never leaves the host.

```sh
augmentagent embeddings fetch-model      # one-time, ~130 MB, SHA-256 verified
augmentagent embeddings info             # provider, model, dim, weights present?
augmentagent embeddings bench            # texts/second and peak memory on this box
```

Weights live under `AUGMENTAGENT_EMBEDDINGS_MODEL_DIR` (default
`~/.local/share/augmentagent/models/<model>`). Nothing fetches them except
`fetch-model`; with weights absent every command that needs them fails with a
message naming it.

## Resource use

- Memory: a few hundred MB while embedding. Batches are bounded by padded
  tokens (`4096`) as well as by count, because attention buffers grow with
  batch × sequence² and ONNX Runtime keeps the peak; without that bound a
  batch of long emails took the process past 2 GB.
- CPU: `AUGMENTAGENT_EMBEDDINGS_THREADS`, default half the cores (more threads
  than physical cores measured slower). A backfill saturates the threads it is
  given; run it with a smaller count or under `nice` on a machine you are using.
- The write lock: model calls run outside transactions; vectors are written in
  short transactions of ~250 rows with a pause between them, and the message
  index is never blocked by a slow model.

## Backfill and maintenance

```sh
augmentagent embeddings chunk                        # (re)build chunks, no model
augmentagent embeddings backfill --max-chunks 5000   # chunk + embed; rerun until remaining = 0
augmentagent embeddings check                        # exit non-zero when vectors are missing/stale
augmentagent embeddings knn "budget spreadsheet" --k 5   # nearest chunks (ids and scores only)
```

`backfill` is resumable: it embeds up to `--max-chunks` chunks whose vector
is missing or whose text changed, then reports `remaining`. Run it in passes
on a large store. The daemon keeps vectors current once
`AUGMENTAGENT_EMBEDDINGS=1` is set: every minute it re-chunks conversations
that gained messages and embeds up to 500 pending chunks.

## Hosted provider (opt-in)

An OpenAI-compatible `/embeddings` API can replace the local model:

```sh
AUGMENTAGENT_EMBEDDINGS_PROVIDER=hosted          # default: local
AUGMENTAGENT_EMBEDDINGS_HOSTED_MODEL=text-embedding-3-large   # default
AUGMENTAGENT_EMBEDDINGS_HOSTED_DIM=1024          # default; recorded with every vector
AUGMENTAGENT_EMBEDDINGS_HOSTED_URL=https://api.openai.com/v1  # default
```

The key is read just-in-time from the keyring slot `augmentagent/api-key` /
`OPENAI_API_KEY` (`augmentagent migrate-secrets-to-keyring` seeds it), or from
an `OPENAI_API_KEY` env var as a fallback. It is never logged and never added
to the daemon's env safelist.

**What leaves the machine:** the prepared text of every chunk you embed (chat
windows, note bodies, email bodies), and every search query. The vendor's
retention and training terms apply to that text; read them before turning
this on. `augmentagent doctor` shows a warning whenever the hosted provider is
selected, so this is never invisible.

Rules the code enforces:

- Selecting `hosted` without a key fails at startup with a message naming the
  keyring slot. It never quietly embeds locally under the hosted label, which
  would mix two vector spaces.
- Requests are batched (64 inputs), retried with backoff on 429/5xx, capped per
  run, and stop on an authentication error without retrying.
- `augmentagent embeddings backfill --dry-run` walks the pending chunks,
  estimates tokens and cost, and sends nothing.
- Vectors record provider, model and dimension. Switching back to local means
  `embeddings backfill` re-embeds under the local model; the hosted vectors
  stay in their own space until you delete them.

## Triage pre-filter (opt-in)

With vectors in place, routine mail can be skipped without a reasoner call
when its nearest already-triaged neighbours are unanimous `skip` and close
enough. Only `skip` is ever emitted; the verdict type cannot express a reply,
draft, flag or approval card.

What it measures: **agreement with the reasoner's own past decisions**. There
is no human-labelled triage corpus, so this preserves current behaviour more
cheaply rather than proving correctness. History-import rows (chat stamped
`digest_only`) are never eligible neighbours, and neither are rows the
pre-filter itself decided.

```sh
augmentagent triage-prefilter calibrate        # time-ordered split, agreement/coverage per threshold
augmentagent triage-prefilter stats            # decisions made, spot-check agreement, auto-disable state
augmentagent triage-prefilter reset            # clear the auto-disable latch
```

Defaults: k=16 unanimous neighbours, similarity ≥ 0.90, no margin gate (from the
reference store's calibration; yours may differ). Enable with
`AUGMENTAGENT_TRIAGE_PREFILTER=1` after reading `calibrate`'s recommendation and setting `AUGMENTAGENT_TRIAGE_PREFILTER_MIN_SIM` (and
optionally `_MIN_MARGIN`, `_K`, `_SPOT_PCT`). A sampled share of decidable
messages (default 10%) still goes to the reasoner; if the recent spot-checks
disagree more than 5%, the pre-filter disables itself and `doctor`/`stats`
say so. Every decision records its neighbours, similarity and threshold
version.
