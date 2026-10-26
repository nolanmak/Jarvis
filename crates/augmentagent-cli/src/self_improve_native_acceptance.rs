//! Paid disposable-repository acceptance probe; never publishes or merges.
//! Run centrally with AUGMENTAGENT_AUTOPR_REVIEWERS=codex and isolated daemon
//! state. An ignored test is a prepared harness, not evidence of a live pass.
use super::*;
use augmentagent_channel_core::ProviderKind::Codex;

async fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("/usr/bin/git")
        .args(args)
        .current_dir(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "fixture git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().into())
}

#[tokio::test]
#[ignore = "paid full native scoper/builder/alignment/reviewer flow; central explicit run only"]
async fn live_native_disposable_scope_build_verify_review_and_seed() -> Result<()> {
    anyhow::ensure!(
        std::env::var("AUGMENTAGENT_AUTOPR_REVIEWERS").as_deref() == Ok("codex"),
        "run with AUGMENTAGENT_AUTOPR_REVIEWERS=codex; this probe must not retry Claude quota"
    );
    let state_home = PathBuf::from(std::env::var_os("XDG_STATE_HOME").context(
        "probe requires an isolated XDG_STATE_HOME under the system temporary directory",
    )?);
    let state_home = state_home.canonicalize()?;
    anyhow::ensure!(
        state_home.starts_with(std::env::temp_dir().canonicalize()?)
            && workflow::path().starts_with(&state_home),
        "probe state and workflow database must be isolated under the temporary directory"
    );
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("candidate");
    std::fs::create_dir(&root)?;
    git(&root, &["init", "-q"]).await?;
    git(&root, &["config", "user.name", "Disposable Autoship Probe"]).await?;
    git(&root, &["config", "user.email", "probe@example.invalid"]).await?;
    std::fs::write(
        root.join("arithmetic.py"),
        "def add(a, b):\n    return a + b - 1\n",
    )?;
    std::fs::write(root.join("rules.rs"),"pub fn checks_complete(done: usize, total: usize) -> bool { total > 0 && done == total }\n")?;
    std::fs::write(root.join("shipping.rs"),"// Empty check sets must never authorize shipping.\npub fn should_ship(done: usize, total: usize) -> bool { crate::rules::checks_complete(done, total) }\n")?;
    git(&root, &["add", "."]).await?;
    git(&root, &["commit", "-qm", "red arithmetic baseline"]).await?;
    let base = git(&root, &["rev-parse", "HEAD"]).await?;
    git(&root, &["update-ref", "refs/remotes/origin/main", &base]).await?;
    let id = uuid::Uuid::new_v4().to_string();
    let repository = format!("autoship-canary/full-{id}");
    let control = workflow::path()
        .parent()
        .context("missing control parent")?
        .join("native-acceptance-probes")
        .join(&id);
    let mut guard = workflow::WorkflowGuard::acquire(workflow::Start {
        repo_root: &root,
        worktree: &root,
        repository: &repository,
        source_issue: Some(840),
        pr: None,
        branch: "disposable",
        contract: "native-acceptance-probe-v1",
        environment: "disposable-python",
    })
    .await?
    .context("probe workflow lease unavailable")?;
    let requirement = "Adding two integer inputs returns their arithmetic sum.";
    let criteria = vec![requirement.to_owned()];
    let issue = Issue {
        number: 840,
        title: "Correct integer addition".into(),
        body: requirement.into(),
        author: "synthetic-probe".into(),
        author_trusted: true,
        research_filed: false,
    };
    // Production scoper selects actual immutable source, then independently
    // authors the contract and verifier. No scripted reviewer stands in here.
    let scope=acceptance::scope_original(&root,&repository,&guard,&format!(
        "Scope issue #840: {requirement} Preserve this criterion verbatim. The only implementation change needed is arithmetic.py. Declare arithmetic.py and rules.rs as allowed FILE paths (rules.rs is reserved for a later diagnostic and must not change in this repair). Supply a Python unittest verifier for integer addition, including mixed signs and zero, with a concrete failure_witness. Do not request Rust builds: Rust files are supporting fixture evidence only.\n{}",acceptance::CONTRACT_PROMPT)).await?;
    let builder =
        augmentagent_channel_core::build_pinned(Codex).context("native Codex unavailable")?;
    let prepared = acceptance::prepare_acceptance(
        &root,
        &root,
        &control,
        840,
        &base,
        &scope,
        builder.as_ref(),
        &[root.clone()],
        &criteria,
        &guard,
    )
    .await?;
    assert_eq!(
        prepared
            .baseline
            .registration
            .baseline_result(&prepared.baseline.receipts)?,
        crate::autoship_contract::BaselineResult::Reproduced
    );
    guard.bind_acceptance(&prepared)?;
    let mut opts = fix_opts(root.clone());
    opts.audit_logger = Some(guard.audit_logger());
    let session = augmentagent_channel_core::native_session::FreshBuilderSessions::new();
    session.scope(builder.call(&opts,"Fix arithmetic.py so add(a,b) returns the arithmetic sum. Use the guarded file tools to edit the actual file. Do not modify verifier files, rules.rs, shipping.rs, or Git metadata. Do not invoke cargo/npm or install anything; controller runs the Python verifier and commits. Return a concise summary after editing.")).await?;
    guard.audit_health().await?;
    let builders = session.observed();
    assert!(
        !builders.is_empty(),
        "no actual native builder transport session observed"
    );
    assert_eq!(std::fs::read_to_string(root.join("rules.rs"))?,"pub fn checks_complete(done: usize, total: usize) -> bool { total > 0 && done == total }\n");
    git(&root, &["add", "."]).await?;
    git(&root, &["commit", "-qm", "native builder candidate"]).await?;
    let mut reference =
        acceptance::verify_and_persist_candidate(&root, &prepared, &control, &[root.clone()])
            .await?;
    guard
        .checkpoint(
            crate::autoship_workflow::Stage::Verify,
            "verifier_passed",
            "map_alignment",
            0,
            &[serde_json::to_string(&reference)?],
        )
        .await?;
    reference.alignment =
        Some(alignment::gate(builder.as_ref(), &issue, &root, &prepared, &control, &guard).await?);
    acceptance::assert_receipt_current(&root, &prepared, &reference).await?;
    guard
        .checkpoint(
            crate::autoship_workflow::Stage::Review,
            "acceptance_passed",
            "review",
            0,
            &[serde_json::to_string(&reference)?],
        )
        .await?;
    let reviewed=aligned_review::run(builder.as_ref(),&issue,root.clone(),&criteria,aligned_review::ReviewContext {
        qa_ok:true,qa_notes:"Controller executed pinned Python acceptance; separate independent QA role still required.",repair_round:0,previous:None,environment_id:"disposable-python",adjudication_attempts_already_made:0,workflow:Some(&guard),acceptance_evidence:Some((&prepared,&reference)),
    }).await;
    let evidence = reviewed
        .structured
        .context("native clean review incomplete")?;
    evidence
        .authority()
        .context("native clean candidate did not earn independent approval")?;
    for pass in &evidence.passes {
        assert!(
            builders
                .iter()
                .all(|actor| actor.id != pass.actor.session_id),
            "review reused builder's actual conversation"
        );
    }
    // Real receipt corruption must be rejected before another paid model call.
    let bytes = std::fs::read(&reference.artifact)?;
    std::fs::write(&reference.artifact, b"corrupt probe artifact")?;
    assert!(
        acceptance::assert_receipt_current(&root, &prepared, &reference)
            .await
            .is_err()
    );
    std::fs::write(&reference.artifact, &bytes)?;
    acceptance::assert_receipt_current(&root, &prepared, &reference).await?;
    // Keep executable arithmetic acceptance green while adding an unrelated
    // seeded regression. The diagnostic path cannot mint shipping authority.
    std::fs::write(
        root.join("rules.rs"),
        "pub fn checks_complete(done: usize, total: usize) -> bool { done >= total }\n",
    )?;
    git(&root, &["add", "rules.rs"]).await?;
    git(
        &root,
        &["commit", "-qm", "seed diagnostic empty-check regression"],
    )
    .await?;
    assert!(
        acceptance::assert_receipt_current(&root, &prepared, &reference)
            .await
            .is_err(),
        "old exact-head evidence accepted after new commit"
    );
    let seeded =
        acceptance::verify_and_persist_candidate(&root, &prepared, &control, &[root.clone()])
            .await?;
    assert!(
        acceptance::assert_receipt_current(&root, &prepared, &seeded)
            .await
            .is_err(),
        "unaligned seeded candidate gained shipping evidence"
    );
    guard
        .checkpoint(
            crate::autoship_workflow::Stage::Review,
            "seeded_diagnostic",
            "review_seed",
            0,
            &[serde_json::to_string(&seeded)?],
        )
        .await?;
    let origin = adoption::origin_for_candidate(&root, Some(&guard)).await?;
    let seed=aligned_review::run_diagnostic_canary(Codex,&[],&aligned_review::ReviewSubject{title:&issue.title,body:&issue.body},root.clone(),&criteria,aligned_review::ReviewContext {
        qa_ok:true,qa_notes:"Arithmetic verifier passed; diagnostic seed is intentionally outside shipping alignment.",repair_round:0,previous:None,environment_id:"disposable-python",adjudication_attempts_already_made:0,workflow:Some(&guard),acceptance_evidence:Some((&prepared,&seeded)),
    },origin).await?.structured.context("native seeded review incomplete")?;
    assert!(
        review_canary::detected_seed(&seed),
        "native reviewer missed seeded boundary: {}",
        serde_json::to_string(&seed)?
    );
    guard.audit_health().await?;
    guard.audit_logger().record_controller(serde_json::json!({"kind":"disposable_native_acceptance_passed","native_builders":builders.iter().map(|b|serde_json::json!({"provider":b.provider.name(),"session_id":b.id,"uncertain":b.uncertain})).collect::<Vec<_>>(),"clean_review":evidence,"seeded_review":seed,"no_remote_publication":true}))?;
    guard.finish()?;
    Ok(())
}
