//! Self-contained text-similarity scorers for the tone-eval harness.
//!
//! Both metrics are deliberately implemented from scratch (no external
//! NLP dep) so the tool stays runnable in any Rust environment without
//! pulling Python or downloading model weights. Results aren't research-
//! grade — they're a "did the change move the needle" sanity check that
//! we can graph over weeks. See #73 §7.

use std::collections::HashMap;

/// BLEU-4 with smoothing-1 (add-one to numerator+denominator). Equivalent
/// to `sentencepiece`'s `corpus_bleu` for n=4 on a single sentence pair.
/// Returns a score in `[0.0, 1.0]`.
pub fn bleu4(candidate: &str, reference: &str) -> f64 {
    let cand: Vec<&str> = tokenize(candidate);
    let refs: Vec<&str> = tokenize(reference);
    if cand.is_empty() || refs.is_empty() {
        return 0.0;
    }

    let mut log_precisions: f64 = 0.0;
    for n in 1..=4 {
        let p = ngram_precision(&cand, &refs, n);
        // Smoothing-1: add-one to both numerator and denominator.
        let smoothed = if p == 0.0 { 1.0 / (cand.len() as f64 + 1.0) } else { p };
        log_precisions += smoothed.ln();
    }
    let geo_mean = (log_precisions / 4.0).exp();
    let bp = brevity_penalty(cand.len(), refs.len());
    geo_mean * bp
}

/// Cosine similarity over bag-of-words term-frequency vectors. Quick and
/// dirty — captures lexical overlap without fancier tf-idf or embeddings.
/// Returns a score in `[0.0, 1.0]`.
pub fn cosine_bow(candidate: &str, reference: &str) -> f64 {
    let cand_vec = term_freqs(candidate);
    let ref_vec = term_freqs(reference);
    if cand_vec.is_empty() || ref_vec.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0;
    for (term, c_freq) in &cand_vec {
        if let Some(r_freq) = ref_vec.get(term) {
            dot += (*c_freq as f64) * (*r_freq as f64);
        }
    }
    let norm = magnitude(&cand_vec) * magnitude(&ref_vec);
    if norm == 0.0 {
        0.0
    } else {
        dot / norm
    }
}

fn tokenize(s: &str) -> Vec<&str> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect()
}

fn term_freqs(s: &str) -> HashMap<String, u32> {
    let mut map: HashMap<String, u32> = HashMap::new();
    for t in tokenize(s) {
        *map.entry(t.to_ascii_lowercase()).or_insert(0) += 1;
    }
    map
}

fn magnitude(v: &HashMap<String, u32>) -> f64 {
    v.values().map(|c| (*c as f64) * (*c as f64)).sum::<f64>().sqrt()
}

fn ngram_precision(cand: &[&str], reference: &[&str], n: usize) -> f64 {
    if cand.len() < n {
        return 0.0;
    }
    let cand_grams = ngrams(cand, n);
    let ref_grams = ngrams(reference, n);
    let mut ref_counts: HashMap<&[&str], u32> = HashMap::new();
    for g in &ref_grams {
        *ref_counts.entry(g.as_slice()).or_insert(0) += 1;
    }
    let mut matches: u32 = 0;
    let mut clipped: HashMap<&[&str], u32> = HashMap::new();
    for g in &cand_grams {
        let cap = ref_counts.get(g.as_slice()).copied().unwrap_or(0);
        let used = clipped.entry(g.as_slice()).or_insert(0);
        if *used < cap {
            *used += 1;
            matches += 1;
        }
    }
    matches as f64 / cand_grams.len() as f64
}

fn ngrams<'a>(tokens: &'a [&'a str], n: usize) -> Vec<Vec<&'a str>> {
    if tokens.len() < n {
        return Vec::new();
    }
    (0..=tokens.len() - n)
        .map(|i| tokens[i..i + n].to_vec())
        .collect()
}

fn brevity_penalty(c_len: usize, r_len: usize) -> f64 {
    if c_len == 0 {
        return 0.0;
    }
    if c_len > r_len {
        1.0
    } else {
        ((1.0 - r_len as f64 / c_len as f64)).exp()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bleu4_identical_strings_is_one() {
        let s = "Friday at 2pm works for me. I'll send a calendar invite.";
        let score = bleu4(s, s);
        assert!(score > 0.99, "expected ~1.0, got {score}");
    }

    #[test]
    fn bleu4_disjoint_strings_is_lower_than_match() {
        let a = "Friday at 2pm works for me.";
        let b = "totally unrelated marketing copy about discount codes.";
        let disjoint = bleu4(a, b);
        let perfect = bleu4(a, a);
        assert!(
            disjoint < perfect / 3.0,
            "expected disjoint << perfect, got {disjoint} vs {perfect}"
        );
    }

    #[test]
    fn bleu4_handles_empty() {
        assert_eq!(bleu4("", "anything"), 0.0);
        assert_eq!(bleu4("anything", ""), 0.0);
    }

    #[test]
    fn cosine_identical_strings_is_one() {
        let s = "Friday at 2pm works for me.";
        let score = cosine_bow(s, s);
        assert!((score - 1.0).abs() < 1e-9, "expected 1.0, got {score}");
    }

    #[test]
    fn cosine_disjoint_is_zero() {
        assert_eq!(
            cosine_bow("alpha beta gamma", "delta epsilon zeta"),
            0.0
        );
    }

    #[test]
    fn cosine_partial_overlap_in_range() {
        let s = cosine_bow("the quick brown fox", "the lazy brown dog");
        assert!(s > 0.0 && s < 1.0, "expected partial overlap, got {s}");
    }

    #[test]
    fn cosine_handles_empty() {
        assert_eq!(cosine_bow("", "anything"), 0.0);
        assert_eq!(cosine_bow("anything", ""), 0.0);
    }
}
