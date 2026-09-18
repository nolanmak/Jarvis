//! Vector storage, incremental embedding and brute-force k-NN (#1130).
//!
//! One row per `(chunk_id, provider, model)`; the stored `text_hash` says
//! which chunk text the vector was computed from, so "needs re-embedding" is
//! a hash comparison, and vectors from different models never mix. Chunk
//! text is not stored: it is recomputed per conversation when embedding.
//!
//! Search is a cosine scan over an in-memory cache of one model's vectors.
//! At conversation-window granularity the corpus is tens of thousands of
//! rows, which a scan handles in milliseconds without a native extension.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use augmentagent_store::rusqlite::{params, Connection, OptionalExtension};
use augmentagent_store::Store;
use serde::Serialize;

use crate::chunk::{self, ChunkParams};
use crate::embedder::{dot, Embedder, ModelId};

/// Rows written per transaction during embedding runs.
pub const WRITE_BATCH_ROWS: usize = 250;

pub fn ensure_tables(c: &Connection) -> augmentagent_store::rusqlite::Result<()> {
    chunk::ensure_tables(c)?;
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS message_vectors (
             chunk_id      TEXT NOT NULL,
             provider      TEXT NOT NULL,
             model         TEXT NOT NULL,
             dim           INTEGER NOT NULL,
             vector        BLOB NOT NULL,
             text_hash     TEXT NOT NULL,
             updated_at_ms INTEGER NOT NULL,
             PRIMARY KEY (chunk_id, provider, model)
         );
         CREATE INDEX IF NOT EXISTS idx_vectors_model ON message_vectors(provider, model);",
    )
}

pub fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// Exact-length decode; a mismatch is an error, never a reinterpretation.
pub fn from_blob(b: &[u8], dim: usize) -> anyhow::Result<Vec<f32>> {
    anyhow::ensure!(
        b.len() == dim * 4,
        "vector blob is {} bytes, expected {} for dim {dim}",
        b.len(),
        dim * 4
    );
    Ok(b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct VectorHealth {
    pub chunks: i64,
    pub vectors: i64,
    /// Chunks with no vector for the active model.
    pub missing: i64,
    /// Chunks whose text changed since their vector was computed.
    pub stale: i64,
    /// Stored rows whose dim disagrees with the active model.
    pub dim_mismatch: i64,
    /// Chunks stored under different tunables than the active ones.
    pub stale_params: i64,
}

impl VectorHealth {
    pub fn is_complete(&self) -> bool {
        self.missing == 0 && self.stale == 0 && self.dim_mismatch == 0 && self.stale_params == 0
    }
}

pub fn check(c: &Connection, id: &ModelId, p: &ChunkParams) -> anyhow::Result<VectorHealth> {
    ensure_tables(c)?;
    let q = |sql: &str,
             params: &[&dyn augmentagent_store::rusqlite::ToSql]|
     -> anyhow::Result<i64> { Ok(c.query_row(sql, params, |r| r.get(0))?) };
    Ok(VectorHealth {
        chunks: q("SELECT COUNT(*) FROM message_chunks", &[])?,
        vectors: q(
            "SELECT COUNT(*) FROM message_vectors WHERE provider = ?1 AND model = ?2",
            &[&id.provider, &id.model],
        )?,
        missing: q(
            "SELECT COUNT(*) FROM message_chunks k WHERE NOT EXISTS (
                 SELECT 1 FROM message_vectors v WHERE v.chunk_id = k.chunk_id
                    AND v.provider = ?1 AND v.model = ?2)",
            &[&id.provider, &id.model],
        )?,
        stale: q(
            "SELECT COUNT(*) FROM message_chunks k JOIN message_vectors v ON v.chunk_id = k.chunk_id
              WHERE v.provider = ?1 AND v.model = ?2 AND v.text_hash <> k.text_hash",
            &[&id.provider, &id.model],
        )?,
        dim_mismatch: q(
            "SELECT COUNT(*) FROM message_vectors WHERE provider = ?1 AND model = ?2 AND dim <> ?3",
            &[&id.provider, &id.model, &(id.dim as i64)],
        )?,
        stale_params: chunk::stale_params_count(c, p)?,
    })
}

/// Conversations that have chunks needing a vector for `id`, with the
/// chunk ids needing work, oldest conversation first. Bounded by `limit`.
fn pending_by_conversation(
    c: &Connection,
    id: &ModelId,
    limit: usize,
) -> augmentagent_store::rusqlite::Result<BTreeMap<String, HashSet<String>>> {
    let mut stmt = c.prepare(
        "SELECT k.conversation_id, k.chunk_id FROM message_chunks k
           LEFT JOIN message_vectors v ON v.chunk_id = k.chunk_id AND v.provider = ?1 AND v.model = ?2
          WHERE v.chunk_id IS NULL OR v.text_hash <> k.text_hash
          ORDER BY k.conversation_id LIMIT ?3",
    )?;
    let mut out: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    for row in stmt.query_map(params![id.provider, id.model, limit as i64], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (cid, chunk_id) = row?;
        out.entry(cid).or_default().insert(chunk_id);
    }
    Ok(out)
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct EmbedReport {
    pub conversations: usize,
    pub embedded: usize,
    /// Chunks still pending after this run (hit `max_chunks`).
    pub remaining: i64,
    pub elapsed_ms: u64,
}

/// Embed every chunk whose vector is missing or stale, up to `max_chunks`.
/// Model calls run outside any transaction; writes go in short IMMEDIATE
/// transactions with a pause between them. Resumable: rerun until
/// `remaining == 0`.
pub fn embed_pending(
    store: &Store,
    embedder: &dyn Embedder,
    p: &ChunkParams,
    max_chunks: usize,
    pause: Duration,
) -> anyhow::Result<EmbedReport> {
    let started = std::time::Instant::now();
    store.with_conn(ensure_tables)?;
    let id = embedder.id().clone();
    let pending = store.with_conn(|c| pending_by_conversation(c, &id, max_chunks.max(1)))?;
    let mut report = EmbedReport {
        conversations: pending.len(),
        ..Default::default()
    };
    let mut batch: Vec<(String, String, Vec<f32>)> = Vec::new(); // chunk_id, text_hash, vector
    let flush = |batch: &mut Vec<(String, String, Vec<f32>)>| -> anyhow::Result<usize> {
        if batch.is_empty() {
            return Ok(0);
        }
        let rows = std::mem::take(batch);
        let n = rows.len();
        store.with_conn(|c| {
            c.execute_batch("BEGIN IMMEDIATE")?;
            let now = chrono_now_ms();
            let mut up = c.prepare(
                "INSERT INTO message_vectors (chunk_id, provider, model, dim, vector, text_hash, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(chunk_id, provider, model) DO UPDATE SET dim = excluded.dim,
                     vector = excluded.vector, text_hash = excluded.text_hash,
                     updated_at_ms = excluded.updated_at_ms",
            )?;
            for (chunk_id, hash, v) in &rows {
                up.execute(params![chunk_id, id.provider, id.model, id.dim as i64, to_blob(v), hash, now])?;
            }
            drop(up);
            c.execute_batch("COMMIT")
        })?;
        if !pause.is_zero() {
            std::thread::sleep(pause);
        }
        Ok(n)
    };
    for (cid, wanted) in &pending {
        // Recompute this conversation's chunks to get their text.
        let chunks = store.with_conn(|c| chunk::compute_chunks(c, cid, p))?;
        let todo: Vec<&chunk::Chunk> = chunks
            .iter()
            .filter(|k| wanted.contains(&k.chunk_id))
            .collect();
        if todo.is_empty() {
            continue;
        }
        let texts: Vec<String> = todo.iter().map(|k| k.text.clone()).collect();
        let vectors = embedder.embed(&texts)?;
        anyhow::ensure!(
            vectors.len() == todo.len(),
            "embedder returned {} vectors for {} texts",
            vectors.len(),
            todo.len()
        );
        for (k, v) in todo.iter().zip(vectors) {
            anyhow::ensure!(
                v.len() == id.dim,
                "embedder returned dim {} for model dim {}",
                v.len(),
                id.dim
            );
            batch.push((k.chunk_id.clone(), k.text_hash.clone(), v));
        }
        if batch.len() >= WRITE_BATCH_ROWS {
            report.embedded += flush(&mut batch)?;
        }
    }
    report.embedded += flush(&mut batch)?;
    report.remaining = store.with_conn(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM message_chunks k
               LEFT JOIN message_vectors v ON v.chunk_id = k.chunk_id AND v.provider = ?1 AND v.model = ?2
              WHERE v.chunk_id IS NULL OR v.text_hash <> k.text_hash",
            params![id.provider, id.model],
            |r| r.get(0),
        )
    })?;
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(report)
}

/// Dry run: walk the same pending set and call the embedder (which, for a
/// hosted provider in dry-run mode, counts tokens and sends nothing) but
/// write no vectors.
pub fn estimate_pending(
    store: &Store,
    embedder: &dyn Embedder,
    p: &ChunkParams,
    max_chunks: usize,
) -> anyhow::Result<EmbedReport> {
    let started = std::time::Instant::now();
    store.with_conn(ensure_tables)?;
    let id = embedder.id().clone();
    let pending = store.with_conn(|c| pending_by_conversation(c, &id, max_chunks.max(1)))?;
    let mut counted = 0usize;
    for (cid, wanted) in &pending {
        let chunks = store.with_conn(|c| chunk::compute_chunks(c, cid, p))?;
        let texts: Vec<String> = chunks
            .iter()
            .filter(|k| wanted.contains(&k.chunk_id))
            .map(|k| k.text.clone())
            .collect();
        if texts.is_empty() {
            continue;
        }
        counted += embedder.embed(&texts)?.len();
    }
    Ok(EmbedReport {
        conversations: pending.len(),
        embedded: 0,
        remaining: counted as i64,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

fn chrono_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// In-memory vectors for one model. Loaded once, refreshed by `reload`.
pub struct VectorCache {
    id: ModelId,
    ids: Vec<String>,
    data: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Neighbour {
    pub chunk_id: String,
    pub score: f32,
}

impl VectorCache {
    pub fn load(c: &Connection, id: &ModelId) -> anyhow::Result<Self> {
        let mut stmt = c.prepare(
            "SELECT chunk_id, vector FROM message_vectors WHERE provider = ?1 AND model = ?2 AND dim = ?3 ORDER BY chunk_id",
        )?;
        let mut ids = Vec::new();
        let mut data = Vec::new();
        for row in stmt.query_map(params![id.provider, id.model, id.dim as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
        })? {
            let (cid, blob) = row?;
            data.extend(from_blob(&blob, id.dim)?);
            ids.push(cid);
        }
        Ok(Self {
            id: id.clone(),
            ids,
            data,
        })
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
    pub fn id(&self) -> &ModelId {
        &self.id
    }
    /// Resident bytes of the vector data.
    pub fn bytes(&self) -> usize {
        self.data.len() * 4 + self.ids.iter().map(|s| s.len()).sum::<usize>()
    }

    /// Cosine k-NN (vectors are unit length). `allowed`, when given, limits
    /// candidates to those chunk ids — the structured filters' result set.
    pub fn knn(
        &self,
        query: &[f32],
        k: usize,
        allowed: Option<&HashSet<String>>,
    ) -> anyhow::Result<Vec<Neighbour>> {
        anyhow::ensure!(
            query.len() == self.id.dim,
            "query dim {} != model dim {}",
            query.len(),
            self.id.dim
        );
        let mut scored: Vec<(f32, usize)> = self
            .ids
            .iter()
            .enumerate()
            .filter(|(_, cid)| allowed.is_none_or(|a| a.contains(cid.as_str())))
            .map(|(i, _)| {
                (
                    dot(query, &self.data[i * self.id.dim..(i + 1) * self.id.dim]),
                    i,
                )
            })
            .collect();
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.1.cmp(&b.1))
        });
        Ok(scored
            .into_iter()
            .take(k)
            .map(|(score, i)| Neighbour {
                chunk_id: self.ids[i].clone(),
                score,
            })
            .collect())
    }
}

/// Chunk ids whose conversation matches the structured filters, for scoping
/// a semantic query the same way `search_messages` scopes keyword hits.
pub fn chunk_ids_matching(
    c: &Connection,
    platform: Option<&str>,
    conv_kind: Option<&str>,
    conversation_id: Option<&str>,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
) -> augmentagent_store::rusqlite::Result<HashSet<String>> {
    let mut stmt = c.prepare(
        "SELECT k.chunk_id FROM message_chunks k
           JOIN message_index mi ON mi.message_id = k.first_message_id
          WHERE (?1 IS NULL OR mi.platform = ?1)
            AND (?2 IS NULL OR mi.conv_kind = ?2)
            AND (?3 IS NULL OR k.conversation_id = ?3)
            AND (?4 IS NULL OR k.end_ts_ms >= ?4)
            AND (?5 IS NULL OR k.start_ts_ms < ?5)",
    )?;
    let rows = stmt.query_map(
        params![platform, conv_kind, conversation_id, since_ms, until_ms],
        |r| r.get::<_, String>(0),
    )?;
    rows.collect()
}

/// Message ids covered by a chunk, for mapping a chunk hit back to messages.
pub fn chunk_range(
    c: &Connection,
    chunk_id: &str,
) -> augmentagent_store::rusqlite::Result<Option<(String, String, String)>> {
    c.query_row(
        "SELECT conversation_id, first_message_id, last_message_id FROM message_chunks WHERE chunk_id = ?1",
        [chunk_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::StubEmbedder;
    use augmentagent_messages::index::drain;
    use augmentagent_store::Email;

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path().join("t.db")).unwrap();
        (d, s)
    }

    fn put(s: &Store, id: &str, platform: &str, thread: &str, from: &str, body: &str, ts: &str) {
        s.upsert_email(&Email {
            message_id: id.into(),
            thread_id: Some(thread.into()),
            from: from.into(),
            to: String::new(),
            cc: String::new(),
            attachments: vec![],
            subject: format!(
                "{}: Pat",
                if platform == "imessage" {
                    "iMessage"
                } else {
                    "WhatsApp"
                }
            ),
            body: body.into(),
            date: ts.into(),
            account_entity_id: None,
            platform: platform.into(),
            kind: "dm".into(),
        })
        .unwrap();
        drain(s, 100, Duration::ZERO).unwrap();
    }

    fn seeded() -> (tempfile::TempDir, Store, ChunkParams) {
        let (d, s) = store();
        let p = ChunkParams::default();
        put(
            &s,
            "a1",
            "imessage",
            "imessage:+15555550100",
            "+15555550100",
            "the budget spreadsheet is ready",
            "2026-01-01T10:00:00Z",
        );
        put(
            &s,
            "a2",
            "imessage",
            "imessage:+15555550100",
            "me",
            "great, reviewing the budget",
            "2026-01-01T10:02:00Z",
        );
        put(
            &s,
            "b1",
            "whatsapp",
            "whatsapp-history:15555550101@s.whatsapp.net",
            "me",
            "dinner at seven tonight?",
            "2026-02-01T18:00:00Z",
        );
        put(
            &s,
            "c1",
            "imessage",
            "imessage:+15555550102",
            "+15555550102",
            "flight lands friday",
            "2026-03-01T09:00:00Z",
        );
        chunk::chunk_all(&s, &p, Duration::ZERO).unwrap();
        (d, s, p)
    }

    #[test]
    fn blob_roundtrip_is_exact_and_wrong_length_is_rejected() {
        let v = vec![0.5f32, -1.25, 3.0e-7, f32::MAX];
        assert_eq!(from_blob(&to_blob(&v), 4).unwrap(), v);
        assert!(from_blob(&to_blob(&v), 3).is_err());
        assert!(from_blob(&[1, 2, 3], 1).is_err());
    }

    #[test]
    fn embed_pending_fills_vectors_and_is_idempotent() {
        let (_d, s, p) = seeded();
        let e = StubEmbedder::new(16);
        let h = s.with_conn(|c| Ok(check(c, e.id(), &p).unwrap())).unwrap();
        assert_eq!((h.chunks, h.vectors, h.missing), (3, 0, 3));
        let r = embed_pending(&s, &e, &p, 1000, Duration::ZERO).unwrap();
        assert_eq!((r.conversations, r.embedded, r.remaining), (3, 3, 0));
        let h = s.with_conn(|c| Ok(check(c, e.id(), &p).unwrap())).unwrap();
        assert!(h.is_complete(), "{h:?}");
        let again = embed_pending(&s, &e, &p, 1000, Duration::ZERO).unwrap();
        assert_eq!(again.embedded, 0, "second run writes nothing");
    }

    #[test]
    fn changed_text_re_embeds_and_deleted_chunks_drop_out_of_health() {
        let (_d, s, p) = seeded();
        let e = StubEmbedder::new(16);
        embed_pending(&s, &e, &p, 1000, Duration::ZERO).unwrap();
        let before: Vec<u8> = s
            .with_conn(|c| c.query_row("SELECT vector FROM message_vectors WHERE chunk_id LIKE 'imessage:+15555550100%'", [], |r| r.get(0)))
            .unwrap();
        // New message extends the open tail: its text hash changes.
        put(
            &s,
            "a3",
            "imessage",
            "imessage:+15555550100",
            "+15555550100",
            "numbers look good",
            "2026-01-01T10:04:00Z",
        );
        s.with_conn(|c| chunk::chunk_conversation(c, "imessage:+15555550100", &p))
            .unwrap();
        let h = s.with_conn(|c| Ok(check(c, e.id(), &p).unwrap())).unwrap();
        assert_eq!(h.stale, 1);
        let r = embed_pending(&s, &e, &p, 1000, Duration::ZERO).unwrap();
        assert_eq!(r.embedded, 1);
        let after: Vec<u8> = s
            .with_conn(|c| c.query_row("SELECT vector FROM message_vectors WHERE chunk_id LIKE 'imessage:+15555550100%'", [], |r| r.get(0)))
            .unwrap();
        assert_ne!(before, after);
        // Deleting the whole conversation removes its chunk; the vector row
        // becomes an orphan that health ignores (no chunk → not missing).
        s.with_conn(|c| c.execute("DELETE FROM emails WHERE messageId IN ('a1','a2','a3')", []))
            .unwrap();
        drain(&s, 10, Duration::ZERO).unwrap();
        chunk::chunk_all(&s, &p, Duration::ZERO).unwrap();
        let h = s.with_conn(|c| Ok(check(c, e.id(), &p).unwrap())).unwrap();
        assert_eq!(h.chunks, 2);
        assert!(h.is_complete());
    }

    #[test]
    fn vectors_are_scoped_by_provider_model_dim() {
        let (_d, s, p) = seeded();
        let a = StubEmbedder::new(16);
        embed_pending(&s, &a, &p, 1000, Duration::ZERO).unwrap();
        let b = StubEmbedder::new(32); // same provider/model name, different dim
        let hb = s.with_conn(|c| Ok(check(c, b.id(), &p).unwrap())).unwrap();
        assert_eq!(
            hb.dim_mismatch, 3,
            "dim disagreement is visible, not silently reused"
        );
        let cache = s
            .with_conn(|c| Ok(VectorCache::load(c, b.id()).unwrap()))
            .unwrap();
        assert!(
            cache.is_empty(),
            "a query with model B never sees model A's rows"
        );
        let other = ModelId {
            provider: "hosted".into(),
            model: "x".into(),
            dim: 16,
        };
        let ho = s.with_conn(|c| Ok(check(c, &other, &p).unwrap())).unwrap();
        assert_eq!((ho.vectors, ho.missing), (0, 3));
    }

    #[test]
    fn knn_returns_nearest_first_and_respects_filters() {
        let (_d, s, p) = seeded();
        let e = StubEmbedder::new(64);
        embed_pending(&s, &e, &p, 1000, Duration::ZERO).unwrap();
        let cache = s
            .with_conn(|c| Ok(VectorCache::load(c, e.id()).unwrap()))
            .unwrap();
        assert_eq!(cache.len(), 3);
        let q = e
            .embed(&["budget spreadsheet review".into()])
            .unwrap()
            .remove(0);
        let hits = cache.knn(&q, 2, None).unwrap();
        assert!(
            hits[0].chunk_id.starts_with("imessage:+15555550100"),
            "{hits:?}"
        );
        assert!(hits[0].score >= hits[1].score);
        let only_whatsapp = s
            .with_conn(|c| chunk_ids_matching(c, Some("whatsapp"), None, None, None, None))
            .unwrap();
        let hits = cache.knn(&q, 5, Some(&only_whatsapp)).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].chunk_id.starts_with("whatsapp-history:"));
        let march = s
            .with_conn(|c| chunk_ids_matching(c, None, None, None, Some(1_772_000_000_000), None))
            .unwrap();
        assert_eq!(march.len(), 1);
        assert!(
            cache.knn(&[0.0; 3], 1, None).is_err(),
            "wrong query dim is an error"
        );
        let (cid, first, last) = s
            .with_conn(|c| chunk_range(c, &hits[0].chunk_id))
            .unwrap()
            .unwrap();
        assert!(cid.starts_with("whatsapp-history:") && first == "b1" && last == "b1");
    }

    #[test]
    fn max_chunks_bounds_a_run_and_it_resumes() {
        let (_d, s, p) = seeded();
        let e = StubEmbedder::new(16);
        let r = embed_pending(&s, &e, &p, 1, Duration::ZERO).unwrap();
        assert_eq!(r.embedded, 1);
        assert_eq!(r.remaining, 2);
        let r = embed_pending(&s, &e, &p, 100, Duration::ZERO).unwrap();
        assert_eq!((r.embedded, r.remaining), (2, 0));
    }

    struct Slow;
    impl Embedder for Slow {
        fn id(&self) -> &ModelId {
            static ID: std::sync::OnceLock<ModelId> = std::sync::OnceLock::new();
            ID.get_or_init(|| ModelId {
                provider: "stub".into(),
                model: "slow".into(),
                dim: 4,
            })
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
            std::thread::sleep(Duration::from_millis(300));
            Ok(texts.iter().map(|_| vec![0.5, 0.5, 0.5, 0.5]).collect())
        }
    }

    #[test]
    fn a_slow_embedder_never_holds_the_write_lock() {
        let (d, s, p) = seeded();
        let path = d.path().join("t.db");
        let worker =
            std::thread::spawn(move || embed_pending(&s, &Slow, &p, 1000, Duration::ZERO).unwrap());
        // A second connection writes while the embedder is "thinking".
        let other = augmentagent_store::rusqlite::Connection::open(&path).unwrap();
        other.busy_timeout(Duration::from_millis(100)).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let mut ok = 0;
        for _ in 0..5 {
            if other
                .execute(
                    "INSERT INTO message_index_queue (message_id) VALUES ('probe-' || random())",
                    [],
                )
                .is_ok()
            {
                ok += 1;
            }
            std::thread::sleep(Duration::from_millis(120));
        }
        let r = worker.join().unwrap();
        assert_eq!(r.embedded, 3);
        assert!(ok >= 4, "writer succeeded {ok}/5 while embedding ran");
    }
}
