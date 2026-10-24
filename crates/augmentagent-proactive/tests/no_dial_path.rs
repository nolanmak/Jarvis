//! #1553 AC4: proactive loops may propose a call, never place one. Dialing
//! lives only in `augmentagent-phone`'s `PhoneService`; this crate must not
//! depend on that crate directly or reach its `Telephony` trait through a
//! re-export in any crate it depends on.
use serde_json::Value;
use std::{collections::BTreeSet, path::Path, process::Command};

const PROACTIVE: &str = "augmentagent-proactive";
const PHONE: &str = "augmentagent-phone";

fn metadata() -> Value {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--locked", "--manifest-path"])
        .arg(&manifest)
        .output()
        .expect("cargo metadata runs");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}
fn package<'a>(meta: &'a Value, id: &str) -> &'a Value {
    meta["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == id)
        .unwrap()
}
fn id_of(meta: &Value, name: &str) -> String {
    meta["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == name && p["source"].is_null())
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}
/// Normal (non-dev, non-build) dependency IDs of one resolved node.
fn normal_deps(meta: &Value, id: &str) -> Vec<String> {
    let node = meta["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == id)
        .unwrap();
    node["deps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| {
            d["dep_kinds"]
                .as_array()
                .unwrap()
                .iter()
                .any(|k| k["kind"].is_null())
        })
        .map(|d| d["pkg"].as_str().unwrap().to_owned())
        .collect()
}
fn rust_sources(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn proactive_has_no_edge_to_the_phone_crate() {
    let meta = metadata();
    let proactive = id_of(&meta, PROACTIVE);
    let phone = id_of(&meta, PHONE);
    assert!(
        !normal_deps(&meta, &proactive).contains(&phone),
        "{PROACTIVE} must not depend on {PHONE}"
    );
    // Nor in dev or build dependencies.
    let declared: Vec<_> = package(&meta, &proactive)["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap().to_owned())
        .collect();
    assert!(!declared.iter().any(|d| d == PHONE), "{declared:?}");
    // The crate's own code never names the phone crate or its dialer.
    let mut files = Vec::new();
    rust_sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        for forbidden in ["augmentagent_phone", "Telephony", "PhoneService"] {
            assert!(!text.contains(forbidden), "{} names {forbidden}", file.display());
        }
    }
}

#[test]
fn no_dependency_of_proactive_re_exports_the_dialer() {
    let meta = metadata();
    let proactive = id_of(&meta, PROACTIVE);
    let phone = id_of(&meta, PHONE);
    // Every workspace crate reachable from proactive over normal dependencies.
    let mut seen = BTreeSet::new();
    let mut stack = vec![proactive.clone()];
    while let Some(id) = stack.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        stack.extend(normal_deps(&meta, &id));
    }
    for id in seen {
        let pkg = package(&meta, &id);
        if id == phone || id == proactive || !pkg["source"].is_null() {
            continue;
        }
        let manifest = Path::new(pkg["manifest_path"].as_str().unwrap());
        let mut files = Vec::new();
        rust_sources(&manifest.parent().unwrap().join("src"), &mut files);
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            for line in text.lines().map(str::trim) {
                if line.starts_with("pub use augmentagent_phone") || line.starts_with("pub extern crate augmentagent_phone") {
                    assert!(
                        !line.contains('*')
                            && !line.contains("Telephony")
                            && !line.contains("PhoneService")
                            && !line.ends_with("augmentagent_phone;"),
                        "{} re-exports the dialer: {line}",
                        file.display()
                    );
                }
            }
        }
    }
}

/// The only capability a proactive loop gets: ask. The trait object carries
/// no dial, grant or provider method.
#[test]
fn call_proposer_is_the_only_phone_capability() {
    use augmentagent_proactive::call_proposal::{CallProposal, CallProposer, ProposalError, ProposalOutcome};
    struct Spy;
    #[async_trait::async_trait]
    impl CallProposer for Spy {
        async fn propose_call(&self, _: CallProposal) -> Result<ProposalOutcome, ProposalError> {
            Ok(ProposalOutcome { deliver_at_ms: 0, deferred: false })
        }
    }
    let _: &dyn CallProposer = &Spy;
}
