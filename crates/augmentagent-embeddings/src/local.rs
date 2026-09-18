//! Local provider: a pinned sentence-embedding model through ONNX Runtime on
//! CPU. Loads only weights that pass the pinned SHA-256; never downloads.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;
use ndarray::Array2;
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Tensor;
use tokenizers::{PaddingParams, Tokenizer, TruncationParams};

use crate::embedder::{l2_normalize, Embedder, Embedding, ModelId};
use crate::fetch;
use crate::model::{thread_count, ModelSpec, DEFAULT_MODEL};

/// Inputs per ONNX run. Small enough that padding waste stays bounded once
/// the batch is sorted by length.
pub const BATCH: usize = 32;
/// Padded tokens per ONNX run. Attention buffers grow with
/// batch × seq², and ONNX Runtime's arena keeps the peak: 32 max-length
/// inputs took the process past 2 GB in QA. With this budget a batch of
/// 512-token inputs is 8 items and the peak stays a few hundred MB.
pub const TOKEN_BUDGET: usize = 4096;

/// Group indices (already sorted by length) into batches bounded by item
/// count and by padded tokens (`max_len × items`). Pure; order preserved.
pub fn plan_batches(lengths: &[usize], max_items: usize, token_budget: usize) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    let mut cur_max = 0usize;
    for (i, &len) in lengths.iter().enumerate() {
        let new_max = cur_max.max(len);
        let would_be = new_max * (cur.len() + 1);
        if !cur.is_empty() && (cur.len() >= max_items.max(1) || would_be > token_budget) {
            out.push(std::mem::take(&mut cur));
            cur_max = 0;
        }
        cur_max = cur_max.max(len);
        cur.push(i);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub struct LocalEmbedder {
    id: ModelId,
    spec: &'static ModelSpec,
    dir: PathBuf,
    tokenizer: Tokenizer,
    // `Session::run` needs `&mut self`; the trait is `&self`, so serialize.
    session: Mutex<Session>,
    threads: usize,
}

impl LocalEmbedder {
    /// Load the default model from its default directory.
    pub fn load_default() -> anyhow::Result<Self> {
        Self::load(&DEFAULT_MODEL, &DEFAULT_MODEL.dir(), thread_count())
    }

    /// Load `spec` from `dir`. Absent or corrupt weights are a typed error
    /// naming the fetch command.
    pub fn load(spec: &'static ModelSpec, dir: &Path, threads: usize) -> anyhow::Result<Self> {
        let bad = fetch::verify(spec, dir)?;
        if !bad.is_empty() {
            return Err(fetch::missing_error(spec, dir));
        }
        let mut tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| anyhow::anyhow!("load tokenizer: {e}"))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: spec.max_tokens,
                ..Default::default()
            }))
            .map_err(|e| anyhow::anyhow!("configure truncation: {e}"))?;
        tokenizer.with_padding(Some(PaddingParams::default()));
        // The builder's error type is not Send, so it can't cross `?` into
        // anyhow; render it to text at each step instead.
        fn ort_err<T, E: std::fmt::Display>(r: Result<T, E>, what: &str) -> anyhow::Result<T> {
            r.map_err(|e| anyhow::anyhow!("{what}: {e}"))
        }
        let builder = ort_err(Session::builder(), "session builder")?;
        let builder = ort_err(
            builder.with_optimization_level(GraphOptimizationLevel::Level3),
            "optimization level",
        )?;
        let builder = ort_err(builder.with_intra_threads(threads.max(1)), "intra threads")?;
        let mut builder = ort_err(builder.with_inter_threads(1), "inter threads")?;
        let session = ort_err(
            builder.commit_from_file(dir.join("model.onnx")),
            "load ONNX model",
        )
        .context("load ONNX model")?;
        Ok(Self {
            id: ModelId {
                provider: "local".into(),
                model: spec.name.into(),
                dim: spec.dim,
            },
            spec,
            dir: dir.to_path_buf(),
            tokenizer,
            session: Mutex::new(session),
            threads: threads.max(1),
        })
    }

    pub fn spec(&self) -> &ModelSpec {
        self.spec
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    pub fn threads(&self) -> usize {
        self.threads
    }

    fn run_batch(&self, texts: &[&str]) -> anyhow::Result<Vec<Embedding>> {
        let enc = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("tokenize: {e}"))?;
        let n = enc.len();
        let len = enc.iter().map(|e| e.get_ids().len()).max().unwrap_or(0);
        let mut ids = Array2::<i64>::zeros((n, len));
        let mut mask = Array2::<i64>::zeros((n, len));
        for (i, e) in enc.iter().enumerate() {
            for (j, (id, m)) in e.get_ids().iter().zip(e.get_attention_mask()).enumerate() {
                ids[[i, j]] = *id as i64;
                mask[[i, j]] = *m as i64;
            }
        }
        let types = Array2::<i64>::zeros((n, len));
        let mut session = self
            .session
            .lock()
            .map_err(|_| anyhow::anyhow!("session poisoned"))?;
        let to_tensor =
            |a: Array2<i64>| Tensor::from_array(a).map_err(|e| anyhow::anyhow!("tensor: {e}"));
        let outputs = session
            .run(ort::inputs![
                "input_ids" => to_tensor(ids)?,
                "attention_mask" => to_tensor(mask)?,
                "token_type_ids" => to_tensor(types)?,
            ])
            .map_err(|e| anyhow::anyhow!("onnx run: {e}"))?;
        // First output: last_hidden_state [batch, seq, dim]; CLS pooling.
        let (shape, data) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow::anyhow!("extract output: {e}"))?;
        let dims: Vec<usize> = shape.iter().map(|d| *d as usize).collect();
        anyhow::ensure!(
            dims.len() == 3 && dims[2] == self.spec.dim,
            "unexpected output shape {dims:?}"
        );
        let (seq, dim) = (dims[1], dims[2]);
        Ok((0..n)
            .map(|i| {
                let start = i * seq * dim;
                let mut v = data[start..start + dim].to_vec();
                l2_normalize(&mut v);
                v
            })
            .collect())
    }
}

impl Embedder for LocalEmbedder {
    fn id(&self) -> &ModelId {
        &self.id
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Embedding>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // Sort by token length so each batch pads to a similar size (measured
        // 2× throughput), bound each batch by items and padded tokens, then
        // restore input order.
        let encodings = self
            .tokenizer
            .encode_batch(texts.iter().map(String::as_str).collect::<Vec<_>>(), true)
            .map_err(|e| anyhow::anyhow!("tokenize: {e}"))?;
        let mut order: Vec<usize> = (0..texts.len()).collect();
        order.sort_by_key(|&i| encodings[i].get_ids().len());
        let lengths: Vec<usize> = order
            .iter()
            .map(|&i| encodings[i].get_ids().len())
            .collect();
        let mut out: Vec<Option<Embedding>> = vec![None; texts.len()];
        for group in plan_batches(&lengths, BATCH, TOKEN_BUDGET) {
            let idx: Vec<usize> = group.iter().map(|&g| order[g]).collect();
            let batch: Vec<&str> = idx.iter().map(|&i| texts[i].as_str()).collect();
            let vectors = self.run_batch(&batch)?;
            for (&i, v) in idx.iter().zip(vectors) {
                out[i] = Some(v);
            }
        }
        Ok(out
            .into_iter()
            .map(|v| v.expect("every input embedded"))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::dot;

    fn weights_dir() -> Option<PathBuf> {
        // `AUGMENTAGENT_EMBEDDINGS_MODEL_DIR` or the default location.
        let d = DEFAULT_MODEL.dir();
        DEFAULT_MODEL.is_present(&d).then_some(d)
    }

    #[test]
    fn long_inputs_form_small_batches_and_short_ones_fill_up() {
        // 512-token inputs: 4096 / 512 = 8 per batch.
        let long = vec![512usize; 20];
        let b = plan_batches(&long, 32, 4096);
        assert_eq!(b.iter().map(Vec::len).collect::<Vec<_>>(), [8, 8, 4]);
        // Short inputs hit the item cap first.
        let short = vec![16usize; 70];
        let b = plan_batches(&short, 32, 4096);
        assert_eq!(b.iter().map(Vec::len).collect::<Vec<_>>(), [32, 32, 6]);
        // Mixed, sorted ascending: budget applies to the padded size.
        let mixed = [10, 10, 100, 100, 500, 500, 500];
        let b = plan_batches(&mixed, 32, 1000);
        for g in &b {
            let max = g.iter().map(|&i| mixed[i]).max().unwrap();
            assert!(max * g.len() <= 1000 || g.len() == 1, "{b:?}");
        }
        let all: Vec<usize> = b.concat();
        assert_eq!(
            all,
            (0..mixed.len()).collect::<Vec<_>>(),
            "order preserved, nothing dropped"
        );
        assert!(plan_batches(&[], 32, 4096).is_empty());
        assert_eq!(
            plan_batches(&[9000], 32, 4096),
            vec![vec![0]],
            "an oversize single item still runs alone"
        );
    }

    #[test]
    fn missing_weights_are_a_typed_error_not_a_download() {
        let d = tempfile::tempdir().unwrap();
        let err = match LocalEmbedder::load(&DEFAULT_MODEL, d.path(), 1) {
            Ok(_) => panic!("loaded with no weights"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("fetch-model"), "{err}");
        assert!(
            std::fs::read_dir(d.path()).unwrap().next().is_none(),
            "nothing was written"
        );
    }

    /// Needs weights: `augmentagent embeddings fetch-model` first.
    #[test]
    #[ignore]
    fn real_model_roundtrip() {
        let dir = weights_dir().expect("weights present");
        let e = LocalEmbedder::load(&DEFAULT_MODEL, &dir, 2).unwrap();
        assert_eq!(e.dim(), 384);
        let v = e
            .embed(&[
                "the quarterly budget spreadsheet is ready".into(),
                "finance numbers doc for this quarter is done".into(),
                "let's grab dinner at seven".into(),
            ])
            .unwrap();
        for x in &v {
            assert_eq!(x.len(), 384);
            assert!((x.iter().map(|a| a * a).sum::<f32>().sqrt() - 1.0).abs() < 1e-4);
        }
        assert!(
            dot(&v[0], &v[1]) > dot(&v[0], &v[2]),
            "paraphrase closer than unrelated"
        );
        let again = e
            .embed(&["the quarterly budget spreadsheet is ready".into()])
            .unwrap();
        assert_eq!(again[0], v[0], "deterministic across calls");
        // Ragged batch with order restoration.
        let many: Vec<String> = (0..70)
            .map(|i| {
                format!(
                    "message number {i} about {}",
                    if i % 2 == 0 { "budget" } else { "dinner" }
                )
            })
            .collect();
        let out = e.embed(&many).unwrap();
        assert_eq!(out.len(), 70);
        assert_eq!(out[3], e.embed(&[many[3].clone()]).unwrap()[0]);
    }
}
