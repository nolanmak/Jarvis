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

A hosted embeddings API can replace the local model. It sends message text to
a third party, so it is never selected implicitly. See the provider issue for
configuration; switching providers re-embeds rather than mixing vector spaces.
