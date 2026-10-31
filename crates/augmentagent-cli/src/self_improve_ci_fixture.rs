//! Compiled test-only CI authority for disposable arithmetic acceptance repos.
//! No environment variable, candidate file, or caller-provided contract selects
//! this authority. It cannot exist in a production binary.
use super::*;

const WORKFLOW_PATH: &str = ".github/workflows/autoship-fixture.yml";
const WORKFLOW: &str = "name: Disposable arithmetic CI\non: [pull_request]\njobs:\n  arithmetic:\n    runs-on: ubuntu-latest\n    steps:\n      - run: python3 -I -B -c \"import runpy; add=runpy.run_path('arithmetic.py')['add']; assert add(2,3)==5; assert add(-2,3)==1\"\n";
const BODY: &str = "python3 -I -B -c \"import runpy; add=runpy.run_path('arithmetic.py')['add']; assert add(2,3)==5; assert add(-2,3)==1\"";
tokio::task_local! { static AUTHORITY_ROOT: PathBuf; }

fn declaration() -> String {
    serde_json::to_string(&serde_json::json!({"version":1,
        "commands":{"autoship-fixture/arithmetic/0":{"body":BODY,"shell":"bash"}},
        "workflows":{WORKFLOW_PATH:{"sha256":digest(WORKFLOW.as_bytes()),"role":"validation",
            "events":{"pull_request":{}},"jobs":{"arithmetic":{"runs-on":"ubuntu-latest","timeout-minutes":10,
                "steps":[{"command_id":"autoship-fixture/arithmetic/0"}]}}}}})).expect("compiled fixture contract")
}

/// Controller fixture construction, before the baseline commit. The file is not
/// a way to select authority: production and unscoped tests ignore it entirely.
pub(in crate::self_improve) fn install(root: &Path) -> Result<()> {
    ensure!(
        root.canonicalize()?.starts_with("/tmp"),
        "disposable fixture must be under /tmp"
    );
    let path = root.join(WORKFLOW_PATH);
    std::fs::create_dir_all(path.parent().unwrap())?;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    use std::io::Write;
    file.write_all(WORKFLOW.as_bytes())?;
    Ok(())
}

pub(in crate::self_improve) async fn scope<F: std::future::Future>(
    root: &Path,
    future: F,
) -> Result<F::Output> {
    let root = root.canonicalize()?;
    ensure!(
        root.starts_with("/tmp"),
        "CI fixture scope cannot authorize a production checkout"
    );
    let common = git(&root, &["rev-parse", "--git-common-dir"]).await?;
    let common = PathBuf::from(common.trim());
    let common = if common.is_absolute() {
        common
    } else {
        root.join(common)
    }
    .canonicalize()?;
    ensure!(
        common.starts_with(&root),
        "CI fixture Git authority must be isolated within fixture root"
    );
    ensure!(
        std::fs::read(root.join(WORKFLOW_PATH))? == WORKFLOW.as_bytes(),
        "compiled fixture workflow missing or changed"
    );
    Ok(AUTHORITY_ROOT.scope(root, future).await)
}

pub(super) fn selected(root: &Path) -> Result<Option<String>> {
    let scoped = AUTHORITY_ROOT.try_with(Clone::clone).ok();
    let Some(scoped) = scoped else {
        return Ok(None);
    };
    ensure!(
        root.canonicalize()?.starts_with(&scoped),
        "CI fixture authority cannot leak to another checkout"
    );
    ensure!(
        std::fs::read(scoped.join(WORKFLOW_PATH))? == WORKFLOW.as_bytes(),
        "compiled fixture workflow changed"
    );
    Ok(Some(declaration()))
}

#[tokio::test]
async fn fixture_authority_is_compiled_scoped_and_cannot_be_selected_by_candidate_files() {
    let directory = tempfile::Builder::new()
        .prefix("ci-fixture-authority-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = directory.path();
    git(root, &["init", "-q"]).await.unwrap();
    install(root).unwrap();
    assert_eq!(contract_for(root).unwrap(), CONTRACT);
    let sibling = root.to_path_buf();
    scope(root, async {
        assert_ne!(contract_for(root).unwrap(), CONTRACT);
        assert_eq!(
            tokio::spawn(async move { contract_for(&sibling).unwrap() })
                .await
                .unwrap(),
            CONTRACT
        );
        let other = tempfile::tempdir().unwrap();
        assert!(contract_for(other.path()).is_err());
        std::fs::write(root.join(WORKFLOW_PATH), "candidate replacement").unwrap();
        assert!(contract_for(root).is_err());
    })
    .await
    .unwrap();
    assert_eq!(contract_for(root).unwrap(), CONTRACT);
}
