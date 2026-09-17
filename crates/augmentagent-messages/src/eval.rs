//! Retrieval-miss eval (#1096) — the gate for any embeddings work.
//!
//! Question sets hold the owner's real questions and the ids of the messages
//! that actually answer them. They are private and live **outside** the repo;
//! this module ships only the schema and the scoring.
//!
//! Reports carry ids, metrics and rubric labels. No field can hold message
//! text, so a report is safe to paste into an issue.

use std::collections::BTreeMap;
use std::path::Path;

use augmentagent_store::rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// One labelled question.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Question {
    pub id: String,
    /// The question as the owner would ask it.
    pub question: String,
    /// Message ids that answer it (labelled by the owner).
    pub relevant: Vec<String>,
    /// Optional structured query, for measuring the tools without a model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
}

/// Why a relevant message was not retrieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissClass {
    /// The message shares no content word with the question: the class
    /// semantic search would address.
    Vocabulary,
    /// A keyword query over the question's own words would have found it, so
    /// the retrieval step asked badly.
    Agent,
    /// Words match but no operator expresses the constraint.
    Structure,
    /// The message is not in the store at all.
    Data,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Miss {
    pub message_id: String,
    pub class: MissClass,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuestionResult {
    pub id: String,
    pub relevant: usize,
    pub retrieved: usize,
    pub hits: usize,
    pub recall_at_k: f64,
    /// Reciprocal rank of the first relevant hit, 0.0 when none.
    pub reciprocal_rank: f64,
    pub tool_calls: usize,
    pub elapsed_ms: u64,
    pub misses: Vec<Miss>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct EvalReport {
    pub questions: usize,
    pub k: usize,
    pub recall_at_k: f64,
    pub mrr: f64,
    pub mean_tool_calls: f64,
    pub mean_elapsed_ms: f64,
    /// Share of questions with at least one `vocabulary` miss — the number
    /// the embeddings decision rule reads.
    pub vocabulary_miss_rate: f64,
    pub misses_by_class: BTreeMap<String, usize>,
    pub per_question: Vec<QuestionResult>,
}

/// What a retrieval step returned for one question.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Retrieval {
    pub message_ids: Vec<String>,
    pub tool_calls: usize,
    pub elapsed_ms: u64,
}

/// A retrieval strategy under test.
pub trait Retriever {
    fn retrieve(
        &mut self,
        conn: &Connection,
        question: &Question,
        k: usize,
    ) -> anyhow::Result<Retrieval>;
}

/// Runs the tools directly: the question's `query` when it has one, else the
/// question text as keywords. Measures the retrieval surface with no model in
/// the loop, which is what CI can run.
#[derive(Debug, Default)]
pub struct ToolRetriever;

impl Retriever for ToolRetriever {
    fn retrieve(&mut self, conn: &Connection, q: &Question, k: usize) -> anyhow::Result<Retrieval> {
        let started = std::time::Instant::now();
        let query = q
            .query
            .clone()
            .unwrap_or_else(|| keyword_query(&q.question));
        let resp = crate::query::search(conn, &query, Some(k), 0)?;
        Ok(Retrieval {
            message_ids: resp.hits.into_iter().map(|h| h.message_id).collect(),
            tool_calls: 1,
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }
}

const STOPWORDS: &[&str] = &[
    "a", "about", "an", "and", "any", "are", "as", "at", "be", "did", "do", "does", "for", "from",
    "had", "has", "have", "how", "i", "in", "is", "it", "its", "last", "me", "my", "of", "on",
    "or", "our", "said", "say", "talk", "talked", "than", "that", "the", "their", "them", "then",
    "there", "they", "this", "to", "was", "we", "were", "what", "when", "where", "which", "who",
    "whom", "why", "with", "you", "your",
];

pub fn content_words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .map(|w| w.to_lowercase())
        .filter(|w| w.len() > 2 && !STOPWORDS.contains(&w.as_str()))
        .collect()
}

fn keyword_query(question: &str) -> String {
    let words = content_words(question);
    if words.is_empty() {
        question.trim().to_string()
    } else {
        words.join(" ")
    }
}

/// Classify one missed message: does it share a content word with the
/// question, is it reachable by those words, is it in the store at all?
fn classify(conn: &Connection, question: &Question, message_id: &str) -> anyhow::Result<MissClass> {
    let rowid: Option<i64> = conn
        .query_row(
            "SELECT rowid FROM message_index WHERE message_id = ?1",
            [message_id],
            |r| r.get(0),
        )
        .ok();
    let Some(rowid) = rowid else {
        return Ok(MissClass::Data);
    };
    let text: String = conn
        .query_row(
            "SELECT COALESCE(title, '') || ' ' || COALESCE(subject, '') || ' ' || COALESCE(body, '') \
             FROM message_fts WHERE rowid = ?1",
            [rowid],
            |r| r.get(0),
        )
        .unwrap_or_default();
    let words = content_words(&text);
    let shared = content_words(&question.question).into_iter().any(|w| {
        words
            .iter()
            .any(|t| t == &w || t.starts_with(&w) || w.starts_with(t.as_str()))
    });
    if !shared {
        return Ok(MissClass::Vocabulary);
    }
    // Words overlap: is the message reachable by ANY single content word of
    // the question? If yes the retrieval step asked badly (a different query
    // would have found it); if no, the constraint needed an operator the
    // grammar doesn't have.
    let reachable = content_words(&question.question).into_iter().any(|word| {
        crate::query::search(conn, &word, Some(50), 0)
            .map(|r| r.hits.iter().any(|h| h.message_id == message_id))
            .unwrap_or(false)
    });
    Ok(if reachable {
        MissClass::Agent
    } else {
        MissClass::Structure
    })
}

pub fn load_questions(path: &Path, repo_root: &Path) -> anyhow::Result<Vec<Question>> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if let Ok(repo) = repo_root.canonicalize() {
        if canonical.starts_with(&repo) {
            anyhow::bail!(
                "question sets hold private message ids and must live outside the repo \
                 (got {})",
                canonical.display()
            );
        }
    }
    let raw = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&raw)?)
}

pub fn run(
    conn: &Connection,
    questions: &[Question],
    k: usize,
    retriever: &mut dyn Retriever,
) -> anyhow::Result<EvalReport> {
    let mut report = EvalReport {
        questions: questions.len(),
        k,
        ..Default::default()
    };
    let mut with_vocabulary_miss = 0usize;
    for q in questions {
        let got = retriever.retrieve(conn, q, k)?;
        let top: Vec<&String> = got.message_ids.iter().take(k).collect();
        let hits = q.relevant.iter().filter(|r| top.contains(r)).count();
        let rr = top
            .iter()
            .position(|g| q.relevant.iter().any(|r| r == *g))
            .map(|i| 1.0 / (i as f64 + 1.0))
            .unwrap_or(0.0);
        let mut misses = Vec::new();
        for missed in q.relevant.iter().filter(|r| !top.contains(r)) {
            misses.push(Miss {
                message_id: missed.clone(),
                class: classify(conn, q, missed)?,
            });
        }
        for m in &misses {
            *report
                .misses_by_class
                .entry(
                    serde_json::to_value(m.class)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_default(),
                )
                .or_default() += 1;
        }
        if misses.iter().any(|m| m.class == MissClass::Vocabulary) {
            with_vocabulary_miss += 1;
        }
        report.recall_at_k += if q.relevant.is_empty() {
            0.0
        } else {
            hits as f64 / q.relevant.len() as f64
        };
        report.mrr += rr;
        report.mean_tool_calls += got.tool_calls as f64;
        report.mean_elapsed_ms += got.elapsed_ms as f64;
        report.per_question.push(QuestionResult {
            id: q.id.clone(),
            relevant: q.relevant.len(),
            retrieved: got.message_ids.len(),
            hits,
            recall_at_k: if q.relevant.is_empty() {
                0.0
            } else {
                hits as f64 / q.relevant.len() as f64
            },
            reciprocal_rank: rr,
            tool_calls: got.tool_calls,
            elapsed_ms: got.elapsed_ms,
            misses,
        });
    }
    let n = questions.len().max(1) as f64;
    report.recall_at_k /= n;
    report.mrr /= n;
    report.mean_tool_calls /= n;
    report.mean_elapsed_ms /= n;
    report.vocabulary_miss_rate = with_vocabulary_miss as f64 / n;
    Ok(report)
}

/// The decision the epic fixed before measuring, applied to a report.
pub fn embeddings_verdict(report: &EvalReport) -> &'static str {
    match report.vocabulary_miss_rate {
        r if r < 0.10 => "no embeddings: vocabulary misses under 10%",
        r if r <= 0.25 => {
            "try cheaper fixes first (alias expansion, agent query rewriting), then re-measure"
        }
        _ => "open an embeddings issue scoped to the classes that missed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::drain;
    use augmentagent_store::{Email, Store};
    use std::time::Duration;

    fn fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db")).unwrap();
        let put = |id: &str, subject: &str, body: &str| {
            store
                .upsert_email(&Email {
                    message_id: id.into(),
                    thread_id: Some("t1".into()),
                    from: "pat@example.com".into(),
                    to: String::new(),
                    cc: String::new(),
                    attachments: vec![],
                    subject: subject.into(),
                    body: body.into(),
                    date: "2026-01-01T00:00:00Z".into(),
                    account_entity_id: None,
                    platform: "gmail".into(),
                    kind: "dm".into(),
                })
                .unwrap();
        };
        put(
            "m-direct",
            "Budget",
            "the quarterly budget spreadsheet is attached",
        );
        put(
            "m-paraphrase",
            "Dinner",
            "you have to try that little place on Elm street",
        );
        put("m-structure", "Quarterly", "numbers look fine");
        drain(&store, 100, Duration::ZERO).unwrap();
        (dir, store)
    }

    fn q(id: &str, question: &str, relevant: &[&str], query: Option<&str>) -> Question {
        Question {
            id: id.into(),
            question: question.into(),
            relevant: relevant.iter().map(|s| s.to_string()).collect(),
            query: query.map(str::to_string),
        }
    }

    #[test]
    fn recall_and_mrr_are_computed_from_ranks() {
        let (_d, s) = fixture();
        let questions = vec![
            q(
                "hit-first",
                "budget spreadsheet",
                &["m-direct"],
                Some("budget"),
            ),
            q(
                "missing",
                "budget spreadsheet",
                &["m-direct", "m-paraphrase"],
                Some("budget"),
            ),
        ];
        let r = s
            .with_conn(|c| Ok(run(c, &questions, 10, &mut ToolRetriever).unwrap()))
            .unwrap();
        assert_eq!(r.per_question[0].recall_at_k, 1.0);
        assert_eq!(r.per_question[0].reciprocal_rank, 1.0);
        assert_eq!(r.per_question[1].recall_at_k, 0.5);
        assert_eq!(r.recall_at_k, 0.75);
        assert_eq!(r.mrr, 1.0);
        assert_eq!(r.per_question[0].tool_calls, 1);
    }

    #[test]
    fn miss_rubric_labels_the_four_classes() {
        let (_d, s) = fixture();
        let questions = vec![
            // Paraphrase: the message shares no content word with the question.
            q(
                "vocab",
                "what restaurant did someone recommend",
                &["m-paraphrase"],
                Some("restaurant"),
            ),
            // Words overlap and a keyword query finds it: the query was bad.
            q(
                "agent",
                "quarterly budget numbers",
                &["m-direct"],
                Some("nonexistentterm"),
            ),
            // Not in the store.
            q("data", "budget", &["m-gone"], Some("budget")),
        ];
        let r = s
            .with_conn(|c| Ok(run(c, &questions, 10, &mut ToolRetriever).unwrap()))
            .unwrap();
        let class = |id: &str| r.per_question.iter().find(|p| p.id == id).unwrap().misses[0].class;
        assert_eq!(class("vocab"), MissClass::Vocabulary);
        assert_eq!(class("agent"), MissClass::Agent);
        assert_eq!(class("data"), MissClass::Data);
        assert_eq!(r.misses_by_class["vocabulary"], 1);
        assert!((r.vocabulary_miss_rate - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn verdict_follows_the_decision_rule_fixed_before_measuring() {
        let with_rate = |rate: f64| EvalReport {
            vocabulary_miss_rate: rate,
            ..Default::default()
        };
        assert!(embeddings_verdict(&with_rate(0.05)).starts_with("no embeddings"));
        assert!(embeddings_verdict(&with_rate(0.20)).contains("cheaper fixes"));
        assert!(embeddings_verdict(&with_rate(0.40)).contains("open an embeddings issue"));
    }

    #[test]
    fn report_cannot_carry_message_text() {
        let (_d, s) = fixture();
        let questions = vec![q(
            "x",
            "budget spreadsheet",
            &["m-direct", "m-paraphrase"],
            Some("budget"),
        )];
        let r = s
            .with_conn(|c| Ok(run(c, &questions, 10, &mut ToolRetriever).unwrap()))
            .unwrap();
        let json = serde_json::to_string(&r).unwrap();
        for text in ["spreadsheet", "Elm", "quarterly budget spreadsheet"] {
            assert!(!json.contains(text), "report leaked message text: {json}");
        }
        assert!(
            json.contains("m-paraphrase"),
            "missed ids are expected in the report"
        );
    }

    #[test]
    fn question_sets_inside_the_repo_are_refused() {
        let repo = tempfile::tempdir().unwrap();
        let inside = repo.path().join("questions.json");
        std::fs::write(&inside, "[]").unwrap();
        let err = load_questions(&inside, repo.path()).unwrap_err();
        assert!(err.to_string().contains("outside the repo"), "{err}");

        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("questions.json");
        std::fs::write(
            &outside,
            r#"[{"id":"q1","question":"budget?","relevant":["m-direct"],"query":"budget"}]"#,
        )
        .unwrap();
        let qs = load_questions(&outside, repo.path()).unwrap();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].relevant, ["m-direct"]);
    }

    #[test]
    fn synthetic_set_runs_with_no_model_and_no_network() {
        // The shape CI runs: fixture store, tool retriever, no model call.
        let (_d, s) = fixture();
        let questions = vec![
            q("a", "budget spreadsheet", &["m-direct"], None),
            q("b", "quarterly numbers", &["m-structure"], None),
        ];
        let r = s
            .with_conn(|c| Ok(run(c, &questions, 10, &mut ToolRetriever).unwrap()))
            .unwrap();
        assert_eq!(r.questions, 2);
        assert!(r.recall_at_k > 0.0);
        assert!(r.mean_elapsed_ms < 5_000.0);
    }
}
