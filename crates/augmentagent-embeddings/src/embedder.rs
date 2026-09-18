//! The provider trait and the deterministic stub used by downstream tests.

use serde::{Deserialize, Serialize};

/// Identifies a vector space. Vectors from different ids are never compared.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelId {
    pub provider: String,
    pub model: String,
    pub dim: usize,
}

impl std::fmt::Display for ModelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}@{}", self.provider, self.model, self.dim)
    }
}

/// L2-normalized `f32` vector.
pub type Embedding = Vec<f32>;

pub trait Embedder: Send + Sync {
    fn id(&self) -> &ModelId;
    /// For provider-specific reporting (e.g. hosted usage) without adding
    /// those concerns to the trait.
    fn as_any(&self) -> &dyn std::any::Any;
    fn dim(&self) -> usize {
        self.id().dim
    }
    /// Embed a batch. Output order matches input order; every vector has
    /// `dim()` entries and unit L2 norm. Implementations pick their own
    /// internal batch size.
    fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Embedding>>;
}

pub fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Cosine similarity of two unit vectors is their dot product.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Deterministic hash-based embedder: same text, same vector, no model.
/// Texts sharing words land near each other, which is enough for pipeline
/// tests. Never used outside tests.
#[cfg(any(test, feature = "stub"))]
#[derive(Debug, Clone)]
pub struct StubEmbedder {
    id: ModelId,
}

#[cfg(any(test, feature = "stub"))]
impl StubEmbedder {
    pub fn new(dim: usize) -> Self {
        Self {
            id: ModelId {
                provider: "stub".into(),
                model: "hash".into(),
                dim,
            },
        }
    }
}

#[cfg(any(test, feature = "stub"))]
impl Embedder for StubEmbedder {
    fn id(&self) -> &ModelId {
        &self.id
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Embedding>> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0.0f32; self.id.dim];
                for word in t
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|w| !w.is_empty())
                {
                    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
                    for b in word.to_lowercase().bytes() {
                        h ^= b as u64;
                        h = h.wrapping_mul(0x0100_0000_01b3);
                    }
                    let idx = (h % self.id.dim as u64) as usize;
                    v[idx] += if (h >> 63) == 0 { 1.0 } else { -1.0 };
                }
                if v.iter().all(|x| *x == 0.0) {
                    v[0] = 1.0;
                }
                l2_normalize(&mut v);
                v
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_embedder_is_deterministic_and_normalized() {
        let e = StubEmbedder::new(64);
        let a = e.embed(&["budget spreadsheet".into()]).unwrap();
        let b = e.embed(&["budget spreadsheet".into()]).unwrap();
        assert_eq!(a, b);
        let norm: f32 = a[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn dim_matches_declared_and_order_is_preserved() {
        let e = StubEmbedder::new(32);
        let out = e
            .embed(&["one".into(), "two".into(), "three".into()])
            .unwrap();
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|v| v.len() == e.dim()));
        assert_ne!(out[0], out[1]);
        assert_eq!(out[1], e.embed(&["two".into()]).unwrap()[0]);
    }

    #[test]
    fn empty_input_returns_empty_and_shared_words_are_closer() {
        let e = StubEmbedder::new(128);
        assert!(e.embed(&[]).unwrap().is_empty());
        let v = e
            .embed(&[
                "budget doc ready".into(),
                "budget doc late".into(),
                "dinner at seven".into(),
            ])
            .unwrap();
        assert!(dot(&v[0], &v[1]) > dot(&v[0], &v[2]));
        assert!(
            e.embed(&["".into()]).unwrap()[0].iter().any(|x| *x != 0.0),
            "empty text still yields a unit vector"
        );
    }

    #[test]
    fn model_id_display_and_equality() {
        let a = ModelId {
            provider: "local".into(),
            model: "m".into(),
            dim: 384,
        };
        let b = ModelId {
            provider: "local".into(),
            model: "m".into(),
            dim: 768,
        };
        assert_ne!(a, b, "dimension is part of the space identity");
        assert_eq!(a.to_string(), "local/m@384");
    }
}
