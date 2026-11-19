//! #2246 — scheduler behaviour driven by per-loop options.
use super::*;
use augmentagent_store::LoopOptions;
use std::collections::VecDeque;
use std::sync::Mutex;

struct Runner {
    replies: Mutex<VecDeque<String>>,
    prompts: Mutex<Vec<String>>,
}
#[async_trait]
impl LoopRunner for Runner {
    async fn run_prompt(&self, _id: &str, _owner: &str, prompt: &str, _model: Option<&str>) -> anyhow::Result<String> {
        self.prompts.lock().unwrap().push(prompt.into());
        Ok(self.replies.lock().unwrap().pop_front().expect("unexpected model call"))
    }
}
#[derive(Default)]
struct Poster {
    posts: Mutex<Vec<String>>,
}
#[async_trait]
impl LoopPoster for Poster {
    async fn post_to(&self, _channel: &str, body: &str) -> anyhow::Result<()> {
        self.posts.lock().unwrap().push(body.into());
        Ok(())
    }
}
#[derive(Default)]
struct Mailer {
    /// (account, to, subject, body)
    sent: Mutex<Vec<(Option<String>, String, String, String)>>,
    /// Fail the send whose 0-based index is this, once.
    fail_at: Mutex<Option<usize>>,
}
#[async_trait]
impl LoopMailer for Mailer {
    async fn send(&self, account: Option<&str>, to: &str, subject: &str, body: &str) -> anyhow::Result<String> {
        let index = self.sent.lock().unwrap().len();
        let mut fail_at = self.fail_at.lock().unwrap();
        if *fail_at == Some(index) {
            *fail_at = None;
            anyhow::bail!("synthetic mail outage");
        }
        self.sent.lock().unwrap().push((account.map(Into::into), to.into(), subject.into(), body.into()));
        Ok("sender@example.com".into())
    }
}

const OWNER: &str = "owner@example.com";

struct Fixture {
    store: Arc<Store>,
    runner: Arc<Runner>,
    poster: Arc<Poster>,
    mailer: Arc<Mailer>,
    scheduler: LoopScheduler,
    _file: tempfile::NamedTempFile,
    wiki: tempfile::TempDir,
}

fn fixture(options: LoopOptions, replies: Vec<String>) -> Fixture {
    let file = tempfile::NamedTempFile::new().unwrap();
    let store = Arc::new(Store::open(file.path()).unwrap());
    let id = store
        .create_user_loop_with_model("owner", "discord", "channel", 60, "Scan for matches", None, None, None, None, false)
        .unwrap();
    store.set_loop_options(&id, &options).unwrap();
    let runner = Arc::new(Runner { replies: Mutex::new(replies.into()), prompts: Mutex::new(vec![]) });
    let poster = Arc::new(Poster::default());
    let mailer = Arc::new(Mailer::default());
    let wiki = tempfile::tempdir().unwrap();
    let mut scheduler = LoopScheduler::new(store.clone(), runner.clone(), poster.clone())
        .with_mailer(mailer.clone())
        .with_wiki_root(wiki.path().to_path_buf());
    scheduler.self_send = Some(vec![OWNER.into()]);
    Fixture { store, runner, poster, mailer, scheduler, _file: file, wiki }
}

impl Fixture {
    fn row(&self) -> UserLoop {
        self.store.list_active_user_loops().unwrap().remove(0)
    }
    async fn run(&self) {
        self.scheduler.run_one(&self.row(), 0).await;
    }
    fn posts(&self) -> Vec<String> {
        self.poster.posts.lock().unwrap().clone()
    }
    fn sent(&self) -> Vec<(Option<String>, String, String, String)> {
        self.mailer.sent.lock().unwrap().clone()
    }
    /// Make the pending delivery retry due now.
    fn skip_backoff(&self) {
        self.store
            .with_conn(|c| c.execute("UPDATE loop_occurrence_failures SET observed_at_ms=observed_at_ms-3600001", []))
            .unwrap();
    }
    fn age_coverage(&self, ms: i64) {
        self.store
            .with_conn(|c| c.execute("UPDATE loop_coverage SET completed_at_ms=completed_at_ms-?1", [ms]))
            .unwrap();
    }
}

fn report(findings: serde_json::Value) -> String {
    serde_json::json!({"status": "complete", "answer": "Scan finished.", "findings": findings}).to_string()
}
fn keyed(key: &str, finding: &str, evidence: &str) -> serde_json::Value {
    serde_json::json!({"key": key, "finding": finding, "evidence": [evidence], "body": format!("Details about {key}.")})
}
fn email_options() -> LoopOptions {
    LoopOptions { email_to: Some(OWNER.into()), ..Default::default() }
}

#[tokio::test]
async fn an_empty_run_still_posts_unless_the_loop_is_quiet() {
    let loud = fixture(LoopOptions::default(), vec![report(serde_json::json!([]))]);
    loud.run().await;
    assert_eq!(loud.posts().len(), 1);

    let quiet = fixture(
        LoopOptions { quiet_when_empty: true, ..Default::default() },
        vec![report(serde_json::json!([]))],
    );
    quiet.run().await;
    assert!(quiet.posts().is_empty());
    let row = quiet.row();
    assert!(row.last_run_ms.is_some(), "a quiet run still completes the occurrence");
    assert_eq!(row.last_status.as_deref(), Some("ok"));
}

#[tokio::test]
async fn a_quiet_loop_still_posts_findings_prose_and_unfinished_runs() {
    let incomplete = serde_json::json!({"status": "incomplete", "answer": "Still working.", "findings": []}).to_string();
    let f = fixture(
        LoopOptions { quiet_when_empty: true, ..Default::default() },
        vec![
            report(serde_json::json!([{"finding": "Fixture changed", "evidence": ["fixture:v1"]}])),
            "plain prose answer".into(),
            incomplete,
        ],
    );
    for _ in 0..3 {
        f.run().await;
    }
    assert_eq!(f.posts().len(), 3);
}

#[tokio::test]
async fn a_keyed_finding_is_reported_once_whatever_its_wording_or_evidence() {
    let f = fixture(
        LoopOptions::default(),
        vec![
            report(serde_json::json!([keyed("Person A", "Person A asked about pricing", "msg:1")])),
            report(serde_json::json!([keyed("person  a", "Pricing question from A, again", "msg:2")])),
        ],
    );
    f.run().await;
    f.run().await;
    assert_eq!(f.posts().len(), 1);
    assert_eq!(f.store.loop_coverage(&f.row().id, 10).unwrap().len(), 1);
}

#[tokio::test]
async fn a_keyed_finding_is_reported_again_only_after_the_window() {
    let again = || report(serde_json::json!([keyed("person-a", "Person A asked about pricing", "msg:1")]));
    let f = fixture(
        LoopOptions { rereport_after_secs: Some(3600), ..Default::default() },
        vec![again(), again(), again(), again()],
    );
    f.run().await;
    f.run().await;
    assert_eq!(f.posts().len(), 1, "inside the window");
    f.age_coverage(3_600_000);
    f.run().await;
    assert_eq!(f.posts().len(), 2, "after the window");
    f.run().await;
    assert_eq!(f.posts().len(), 2, "the window restarts at the latest report");
}

#[tokio::test]
async fn covered_keys_are_listed_in_the_next_prompt_only_for_keyed_loops() {
    let reply = || report(serde_json::json!([keyed("person-a", "Person A asked about pricing", "msg:1")]));
    let keyed_loop = fixture(
        LoopOptions { rereport_after_secs: Some(3600), ..Default::default() },
        vec![reply(), reply()],
    );
    keyed_loop.run().await;
    keyed_loop.run().await;
    let prompts = keyed_loop.runner.prompts.lock().unwrap().clone();
    assert!(prompts[0].contains("\"key\""));
    assert!(!prompts[0].contains("do not report these"));
    assert!(prompts[1].contains("do not report these): person-a."));

    let plain = fixture(LoopOptions::default(), vec![reply()]);
    plain.run().await;
    let prompt = plain.runner.prompts.lock().unwrap()[0].clone();
    assert!(prompt.ends_with("keep the requested reminder in answer even when unchanged."), "a loop without options keeps the original prompt");
}

#[tokio::test]
async fn each_new_finding_is_emailed_once_and_the_channel_gets_a_receipt() {
    let f = fixture(
        LoopOptions { email_account: Some("sender@example.com".into()), ..email_options() },
        vec![
            report(serde_json::json!([
                keyed("person-a", "Person A asked about pricing", "msg:1"),
                keyed("person-b", "Person B asked about pricing", "msg:2"),
            ])),
            report(serde_json::json!([
                keyed("person-a", "Person A asked again", "msg:3"),
                keyed("person-c", "Person C asked about pricing", "msg:4"),
            ])),
        ],
    );
    f.run().await;
    let sent = f.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].0.as_deref(), Some("sender@example.com"));
    assert_eq!(sent[0].1, OWNER);
    assert_eq!(sent[0].2, "Person A asked about pricing");
    assert!(sent[0].3.starts_with("Details about person-a."));
    assert!(sent[0].3.contains("Evidence:\n- msg:1"));
    let posts = f.posts();
    assert_eq!(posts.len(), 1);
    assert!(posts[0].contains("Emailed 2 new findings to owner@example.com:"));
    assert!(posts[0].contains("- Person B asked about pricing"));
    assert!(!posts[0].contains("Details about"), "the receipt never carries email bodies");

    f.run().await;
    let sent = f.sent();
    assert_eq!(sent.len(), 3, "only the new subject is emailed");
    assert_eq!(sent[2].2, "Person C asked about pricing");
    assert!(f.posts()[1].contains("Emailed 1 new finding to owner@example.com:"));
}

#[tokio::test]
async fn every_loop_email_is_recorded_so_its_inbound_copy_is_recognised() {
    let f = fixture(email_options(), vec![report(serde_json::json!([keyed("person-a", "Person A asked", "msg:1")]))]);
    f.run().await;
    let (_, _, subject, body) = f.sent().remove(0);
    assert!(f.store.is_loop_delivery_email("Sender <sender@example.com>", &subject, &body).unwrap());
    assert!(!f.store.is_loop_delivery_email("Sender <sender@example.com>", &format!("Re: {subject}"), &body).unwrap());
}

#[tokio::test]
async fn an_address_off_the_allowlist_is_refused_and_recorded() {
    let mut f = fixture(email_options(), vec![report(serde_json::json!([keyed("person-a", "Person A asked", "msg:1")]))]);
    f.scheduler.self_send = Some(vec!["someone-else@example.com".into()]);
    f.run().await;
    assert!(f.sent().is_empty());
    assert!(f.posts().is_empty());
    let row = f.row();
    assert!(row.last_run_ms.is_some(), "a refusal is not retried as a delivery");
    assert!(row.last_status.unwrap().contains("email delivery refused"));
    assert!(f.store.loop_coverage(&row.id, 10).unwrap().is_empty());
}

#[tokio::test]
async fn an_empty_allowlist_or_a_missing_mailer_refuses_delivery() {
    let reply = || report(serde_json::json!([keyed("person-a", "Person A asked", "msg:1")]));
    let mut off = fixture(email_options(), vec![reply()]);
    off.scheduler.self_send = Some(vec![]);
    off.run().await;
    assert!(off.sent().is_empty());
    assert!(off.row().last_status.unwrap().contains("email delivery refused"));

    let mut no_mailer = fixture(email_options(), vec![reply()]);
    no_mailer.scheduler.mailer = None;
    no_mailer.run().await;
    assert!(no_mailer.row().last_status.unwrap().contains("no mail account"));
}

#[tokio::test]
async fn a_failed_send_retries_only_what_was_not_delivered_without_a_second_model_call() {
    let f = fixture(
        email_options(),
        vec![report(serde_json::json!([
            keyed("person-a", "Person A asked", "msg:1"),
            keyed("person-b", "Person B asked", "msg:2"),
        ]))],
    );
    *f.mailer.fail_at.lock().unwrap() = Some(1);
    f.run().await;
    assert_eq!(f.sent().len(), 1);
    assert!(f.posts().is_empty());
    assert!(f.row().last_run_ms.is_none(), "the occurrence stays open for retry");

    f.skip_backoff();
    f.run().await;
    let sent = f.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!((sent[0].2.as_str(), sent[1].2.as_str()), ("Person A asked", "Person B asked"));
    assert_eq!(f.runner.prompts.lock().unwrap().len(), 1);
    assert!(f.posts()[0].contains("Emailed 1 new finding"));
    assert!(f.row().last_run_ms.is_some());
}

#[tokio::test]
async fn the_prompt_file_is_read_at_every_run() {
    let f = fixture(
        LoopOptions { prompt_file: Some("scans/criteria.md".into()), ..Default::default() },
        vec!["first".into(), "second".into()],
    );
    std::fs::create_dir_all(f.wiki.path().join("scans")).unwrap();
    std::fs::write(f.wiki.path().join("scans/criteria.md"), "Look for pricing questions.\n").unwrap();
    f.run().await;
    std::fs::write(f.wiki.path().join("scans/criteria.md"), "Look for refund questions.\n").unwrap();
    f.run().await;
    let prompts = f.runner.prompts.lock().unwrap().clone();
    assert!(prompts[0].starts_with("Scan for matches\n\nLook for pricing questions.\n\nScheduled progress context"));
    assert!(prompts[1].starts_with("Scan for matches\n\nLook for refund questions."));
}

#[tokio::test]
async fn a_missing_empty_or_escaping_prompt_file_fails_the_run_without_a_model_call() {
    for (path, write, expect) in [
        ("scans/missing.md", None, "not found"),
        ("scans/empty.md", Some("  \n"), "is empty"),
        ("../outside.md", None, "relative path inside the wiki"),
        ("/etc/hosts", None, "relative path inside the wiki"),
    ] {
        let f = fixture(LoopOptions { prompt_file: Some(path.into()), ..Default::default() }, vec![]);
        std::fs::create_dir_all(f.wiki.path().join("scans")).unwrap();
        if let Some(text) = write {
            std::fs::write(f.wiki.path().join(path), text).unwrap();
        }
        f.run().await;
        assert!(f.runner.prompts.lock().unwrap().is_empty(), "{path}");
        assert!(f.posts().is_empty(), "{path}");
        let row = f.row();
        assert!(row.last_run_ms.is_some(), "{path}: the schedule advances");
        assert!(row.last_status.as_deref().unwrap().contains(expect), "{path}: {:?}", row.last_status);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_prompt_file_symlink_out_of_the_wiki_is_refused() {
    let f = fixture(LoopOptions { prompt_file: Some("link.md".into()), ..Default::default() }, vec![]);
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(outside.path(), "outside instructions").unwrap();
    std::os::unix::fs::symlink(outside.path(), f.wiki.path().join("link.md")).unwrap();
    f.run().await;
    assert!(f.runner.prompts.lock().unwrap().is_empty());
    assert!(f.row().last_status.unwrap().contains("outside the wiki"));
}

#[test]
fn only_one_plain_allowlisted_address_may_be_sent_to() {
    let allow = parse_self_send_allowlist(" Owner@Example.com , ,second@example.com ");
    assert_eq!(allow, vec!["owner@example.com", "second@example.com"]);
    assert!(self_send_allowed("owner@example.com", &allow));
    assert!(self_send_allowed(" OWNER@example.com ", &allow));
    for refused in [
        "stranger@example.com",
        "owner@example.com, stranger@example.com",
        "owner@example.com;stranger@example.com",
        "Owner <owner@example.com>",
        "owner@example.com stranger@example.com",
        "",
    ] {
        assert!(!self_send_allowed(refused, &allow), "{refused}");
    }
    assert!(!self_send_allowed("owner@example.com", &[]));
}
