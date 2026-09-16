//! Source inventory: adding a production preset requires an explicit contract.
use std::collections::BTreeSet;
use std::path::Path;
use syn::visit::{self, Visit};

fn test_only(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().any(|attribute| attribute.path().is_ident("test")
        || (attribute.path().is_ident("cfg") && attribute.parse_args::<syn::Path>()
            .is_ok_and(|path| path.is_ident("test"))))
}

#[derive(Default)]
struct Inventory {
    scope: Vec<String>,
    presets: BTreeSet<String>,
}

impl Inventory {
    fn record(&mut self) { self.presets.insert(self.scope.join("::")); }
    fn returns_opts(&mut self, signature: &syn::Signature) {
        if let syn::ReturnType::Type(_, ty) = &signature.output {
            if let syn::Type::Path(path) = &**ty {
                if path.path.segments.last().is_some_and(|segment| segment.ident == "ReasonerOpts") {
                    self.record();
                }
            }
        }
    }
    fn policy_field(expression: &syn::Expr) -> bool {
        matches!(expression, syn::Expr::Field(field) if matches!(&field.member,
            syn::Member::Named(name) if name == "allowed_tools" || name == "settings_json"))
    }
}

impl<'ast> Visit<'ast> for Inventory {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if test_only(&item.attrs) { return; }
        self.scope.push(item.ident.to_string());
        visit::visit_item_mod(self, item);
        self.scope.pop();
    }
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if test_only(&item.attrs) { return; }
        self.scope.push(item.sig.ident.to_string());
        self.returns_opts(&item.sig);
        visit::visit_item_fn(self, item);
        self.scope.pop();
    }
    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if test_only(&item.attrs) { return; }
        self.scope.push(item.sig.ident.to_string());
        self.returns_opts(&item.sig);
        visit::visit_impl_item_fn(self, item);
        self.scope.pop();
    }
    fn visit_expr_struct(&mut self, item: &'ast syn::ExprStruct) {
        if item.path.segments.last().is_some_and(|segment| segment.ident == "ReasonerOpts") {
            self.record();
        }
        visit::visit_expr_struct(self, item);
    }
    fn visit_expr_assign(&mut self, item: &'ast syn::ExprAssign) {
        if Self::policy_field(&item.left) { self.record(); }
        visit::visit_expr_assign(self, item);
    }
    fn visit_expr_method_call(&mut self, item: &'ast syn::ExprMethodCall) {
        if Self::policy_field(&item.receiver) && matches!(item.method.to_string().as_str(),
            "push" | "extend" | "retain" | "clear" | "insert" | "remove" | "append") {
            self.record();
        }
        visit::visit_expr_method_call(self, item);
    }
}

fn collect(directory: &Path, root: &Path, found: &mut BTreeSet<String>) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() { collect(&path, root, found); }
        else if path.extension().is_some_and(|extension| extension == "rs") {
            let source = std::fs::read_to_string(&path).unwrap();
            let mut inventory = Inventory::default();
            inventory.visit_file(&syn::parse_file(&source).unwrap());
            let relative = path.strip_prefix(root).unwrap().to_string_lossy();
            for name in inventory.presets { found.insert(format!("{relative}::{name}")); }
        }
    }
}

#[test]
fn inventory_accounts_for_every_production_preset_and_wrapper() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let mut found = BTreeSet::new();
    for entry in std::fs::read_dir(root.join("crates")).unwrap() {
        let src = entry.unwrap().path().join("src");
        if src.is_dir() { collect(&src, &root, &mut found); }
    }
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("docs/reasoner-capabilities.json")).unwrap()).unwrap();
    let entries = manifest["presets"].as_array().unwrap();
    let mut declared = BTreeSet::new();
    for entry in entries {
        let callsite = entry["callsite"].as_str().unwrap();
        assert!(declared.insert(callsite.to_string()), "duplicate preset: {callsite}");
        for field in ["permission_profile", "output_contract"] {
            assert!(entry[field].as_str().is_some_and(|value| !value.is_empty()), "missing {field}: {callsite}");
        }
        assert!(entry["conformance"].is_array(), "missing conformance tracking: {callsite}");
        let profile = entry["permission_profile"].as_str().unwrap();
        assert!(manifest["profiles"][profile].is_object(), "unknown permission profile: {profile}");
    }
    assert_eq!(found, declared, "production presets changed; update the capability contract and conformance coverage");
}

#[test]
fn core_presets_match_permission_contracts() {
    use augmentagent_channel_core::{reasoner::*, providers::{classify, CapabilityClass::*}, codex_tools::BridgeLaunch};
    let fixture = tempfile::tempdir().unwrap();
    let wiki = fixture.path().join("wiki");
    std::fs::create_dir(&wiki).unwrap();
    let presets = vec![
        ("triage_opts", triage_opts(Some(wiki.clone())), ReadTools, false),
        ("draft_opts", draft_opts("Synthetic draft instructions".into(), Some(wiki.clone())), ReadTools, false),
        ("digest_opts", digest_opts(Some(wiki.clone())), ReadTools, false),
        ("lint_opts", lint_opts("Synthetic lint instructions".into(), wiki.clone()), ReadTools, false),
        ("wiki_migrate_opts", wiki_migrate_opts("Synthetic migration instructions".into(), wiki.clone()), ReadTools, false),
        ("ingest_opts", ingest_opts("Synthetic ingestion instructions".into(), wiki.clone()), WriteTools, true),
        ("resume_opts", resume_opts(wiki.clone()), WriteTools, true),
        ("ask_opts", ask_opts(wiki.clone(), fixture.path().into()), FullAgentic, true),
        ("tone_summarize_opts", tone_summarize_opts(), TextOnly, false),
        ("social_adapter_opts", social_adapter_opts("Synthetic adaptation instructions".into()), TextOnly, false),
        ("loop_parse_opts", loop_parse_opts(), TextOnly, false),
        ("archetype_pick_opts", archetype_pick_opts(), TextOnly, false),
    ];
    let manifest: serde_json::Value = serde_json::from_str(include_str!("../../../docs/reasoner-capabilities.json")).unwrap();
    for (name, opts, class, writable) in presets {
        assert_eq!(classify(&opts), class, "{name}: capability changed");
        let launch_dir = fixture.path().join(name);
        std::fs::create_dir(&launch_dir).unwrap();
        let launch = BridgeLaunch::prepare(&opts, &launch_dir).unwrap();
        let policy: serde_json::Value = serde_json::from_slice(&std::fs::read(launch.policy_path).unwrap()).unwrap();
        assert_eq!(policy["allowed_tools"], serde_json::json!(opts.allowed_tools), "{name}: tools dropped");
        let writes = policy["write_roots"].as_array().unwrap();
        assert_eq!(!writes.is_empty(), writable, "{name}: wrong write scope");
        if writable { assert_eq!(writes, &[serde_json::json!(wiki)], "{name}: writes escaped wiki"); }
        let site = format!("crates/augmentagent-channel-core/src/reasoner.rs::{name}");
        let entry = manifest["presets"].as_array().unwrap().iter().find(|entry| entry["callsite"] == site).unwrap();
        assert!(entry["conformance"].as_array().unwrap().iter().any(|test|
            test == "capability_inventory::core_presets_match_permission_contracts"));
    }
    for opts in [triage_opts(None), draft_opts("Synthetic".into(), None), digest_opts(None)] {
        assert_eq!(classify(&opts), TextOnly, "optional wiki context must not grant tools when absent");
    }
}

#[test]
fn optional_mcp_profiles_preserve_readonly_guard_and_private_auth() {
    use augmentagent_channel_core::{reasoner::*, providers::{classify, CapabilityClass}, codex_tools::BridgeLaunch};
    use std::process::Command;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    // Isolate feature flags and fixture credentials from concurrent tests.
    if std::env::var_os("JARVIS_MCP_CONTRACT_CHILD").is_none() {
        for enabled in ["0", "1"] {
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "optional_mcp_profiles_preserve_readonly_guard_and_private_auth"])
                .current_dir(&root)
                .env("JARVIS_MCP_CONTRACT_CHILD", enabled)
                .env("AUGMENTAGENT_SOCIALAPI_MCP_READONLY", enabled)
                .env("AUGMENTAGENT_SOCIALAPI_MCP_URL", "http://127.0.0.1:1/synthetic-mcp")
                .env("SOCIALAPI_API_KEY", "fixture")
                .output().unwrap();
            assert!(output.status.success(), "{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        }
        return;
    }
    let enabled = std::env::var("JARVIS_MCP_CONTRACT_CHILD").unwrap() == "1";
    let fixture = tempfile::tempdir().unwrap();
    let wiki = fixture.path().join("wiki");
    std::fs::create_dir(&wiki).unwrap();
    let presets = [
        socialapi_draft_opts("Synthetic draft".into(), Some(wiki.clone())),
        with_socialapi_readonly_mcp(draft_opts("Synthetic draft".into(), Some(wiki)), &root),
    ];
    for (index, opts) in presets.into_iter().enumerate() {
        assert_eq!(classify(&opts), if enabled { CapabilityClass::FullAgentic } else { CapabilityClass::ReadTools });
        let launch_dir = fixture.path().join(index.to_string());
        std::fs::create_dir(&launch_dir).unwrap();
        let launch = BridgeLaunch::prepare(&opts, &launch_dir).unwrap();
        let policy: serde_json::Value = serde_json::from_slice(&std::fs::read(&launch.policy_path).unwrap()).unwrap();
        assert_eq!(policy["write_roots"], serde_json::json!([]));
        if enabled {
            assert_eq!(policy["environment"]["SOCIALAPI_API_KEY"], "fixture");
            assert_eq!(policy["settings"], serde_json::from_str::<serde_json::Value>(opts.settings_json.as_ref().unwrap()).unwrap());
            let probe = r#"
import json, runpy, sys
bridge = runpy.run_path(sys.argv[1])
policy = bridge['Policy'](json.load(open(sys.argv[2])))
policy.before('mcp__socialapi__get_post', {})
try:
    policy.before('mcp__socialapi__create_post', {})
except bridge['Denied']:
    pass
else:
    raise AssertionError('write operation escaped original read-only guard')
"#;
            let output = Command::new("python3").args(["-I", "-c", probe])
                .arg(launch_dir.join("tool-bridge.py")).arg(&launch.policy_path).output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        } else {
            assert_eq!(policy["settings"], serde_json::json!({}));
        }
    }
}

#[test]
fn scanner_handles_presets_after_test_modules_and_opt_in_wrappers() {
    let source = r#"
        #[cfg(test)] mod tests { fn fixture() -> ReasonerOpts { todo!() } }
        pub fn resume_opts() -> ReasonerOpts { todo!() }
        #[cfg(feature = "integration")] fn optional(opts: ReasonerOpts) -> ReasonerOpts { opts }
        impl Channel { async fn handle(&self) { let opts = ReasonerOpts { tools: vec![] }; } }
        fn mutate(opts: &mut ReasonerOpts) { opts.allowed_tools.retain(|tool| tool != "Write"); }
    "#;
    let mut inventory = Inventory::default();
    inventory.visit_file(&syn::parse_file(source).unwrap());
    assert_eq!(inventory.presets, BTreeSet::from(["resume_opts".into(), "optional".into(), "handle".into(), "mutate".into()]));
}
