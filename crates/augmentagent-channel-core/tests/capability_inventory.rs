//! Source inventory: adding a production preset requires an explicit contract.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use syn::visit::{self, Visit};

fn test_only(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().any(|attribute| attribute.path().is_ident("test")
        || (attribute.path().is_ident("cfg") && attribute.parse_args::<syn::Path>()
            .is_ok_and(|path| path.is_ident("test"))))
}

/// What the scanner learned about one production callsite.
#[derive(Default, Debug)]
struct Site {
    /// Builds a `ReasonerOpts` (a literal naming `model`, or
    /// `ReasonerOpts::pinned`) rather than wrapping or narrowing one built
    /// elsewhere.
    constructs: bool,
    /// #448/#1046: a literal `model: None` or an `opts.model = None`. No
    /// `--model` flag is emitted, so the spawned CLI inherits the owner's
    /// interactive `~/.claude/settings.json` model and quota.
    unpinned: bool,
    /// Tiers the source states: the `ModelTier` passed to
    /// `ReasonerOpts::pinned`, or what a literal model id implies (haiku is
    /// fast, anything else quality, as in `providers::tier_of`).
    tiers: BTreeSet<String>,
}

#[derive(Default)]
struct Inventory {
    scope: Vec<String>,
    presets: BTreeMap<String, Site>,
}

impl Inventory {
    fn record(&mut self) -> &mut Site { self.presets.entry(self.scope.join("::")).or_default() }
    fn returns_opts(&mut self, signature: &syn::Signature) {
        if let syn::ReturnType::Type(_, ty) = &signature.output {
            if let syn::Type::Path(path) = &**ty {
                if path.path.segments.last().is_some_and(|segment| segment.ident == "ReasonerOpts") {
                    self.record();
                }
            }
        }
    }
    fn named_field(expression: &syn::Expr, names: &[&str]) -> bool {
        matches!(expression, syn::Expr::Field(field) if matches!(&field.member,
            syn::Member::Named(name) if names.iter().any(|wanted| name == wanted)))
    }
    fn policy_field(expression: &syn::Expr) -> bool {
        Self::named_field(expression, &["allowed_tools", "settings_json"])
    }
    fn is_none(expression: &syn::Expr) -> bool {
        matches!(expression, syn::Expr::Path(path) if path.path.is_ident("None"))
    }
    /// The model id in `Some("claude-…".into())`-shaped literals; `None` when
    /// the model comes from a helper or env lookup (checked at runtime instead).
    fn literal_model(expression: &syn::Expr) -> Option<String> {
        match expression {
            syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(value), .. }) => Some(value.value()),
            syn::Expr::Call(call) if matches!(&*call.func,
                syn::Expr::Path(path) if path.path.is_ident("Some")) => call.args.first().and_then(Self::literal_model),
            syn::Expr::MethodCall(call) if call.args.is_empty() => Self::literal_model(&call.receiver),
            _ => None,
        }
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
            let model = item.fields.iter().find(|field| matches!(&field.member,
                syn::Member::Named(name) if name == "model")).map(|field| &field.expr);
            let site = self.record();
            if let Some(model) = model {
                site.constructs = true;
                if Self::is_none(model) {
                    site.unpinned = true;
                } else if let Some(id) = Self::literal_model(model) {
                    site.tiers.insert(if id.to_ascii_lowercase().contains("haiku") { "fast" } else { "quality" }.into());
                }
            }
        }
        visit::visit_expr_struct(self, item);
    }
    fn visit_expr_call(&mut self, item: &'ast syn::ExprCall) {
        // #1046: `ReasonerOpts::pinned(tier, prompt)` is a construction whose
        // tier is spelled at the call site.
        if let syn::Expr::Path(path) = &*item.func {
            let segments: Vec<String> = path.path.segments.iter().rev().take(2)
                .map(|segment| segment.ident.to_string()).collect();
            if segments == ["pinned", "ReasonerOpts"] {
                // Only a spelled-out `ModelTier::Quality` / `ModelTier::Fast`
                // can be checked against the manifest; a variable cannot.
                let tier = match item.args.first() {
                    Some(syn::Expr::Path(tier)) => {
                        let names: Vec<String> = tier.path.segments.iter()
                            .map(|segment| segment.ident.to_string()).collect();
                        match names.as_slice() {
                            [.., kind, name] if kind == "ModelTier" => Some(name.to_ascii_lowercase()),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                let site = self.record();
                site.constructs = true;
                site.tiers.insert(tier.unwrap_or_else(|| "unstated at the call site".into()));
            }
        }
        visit::visit_expr_call(self, item);
    }
    fn visit_expr_assign(&mut self, item: &'ast syn::ExprAssign) {
        if Self::policy_field(&item.left) { self.record(); }
        if Self::named_field(&item.left, &["model"]) && Self::is_none(&item.right) {
            self.record().unpinned = true;
        }
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

fn collect(directory: &Path, root: &Path, found: &mut BTreeMap<String, Site>) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() { collect(&path, root, found); }
        else if path.extension().is_some_and(|extension| extension == "rs") {
            let source = std::fs::read_to_string(&path).unwrap();
            let mut inventory = Inventory::default();
            inventory.visit_file(&syn::parse_file(&source).unwrap());
            let relative = path.strip_prefix(root).unwrap().to_string_lossy();
            for (name, site) in inventory.presets { found.insert(format!("{relative}::{name}"), site); }
        }
    }
}

/// Every production callsite in the workspace, plus the checked-in manifest.
fn scan_workspace() -> (BTreeMap<String, Site>, serde_json::Value) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let mut found = BTreeMap::new();
    for entry in std::fs::read_dir(root.join("crates")).unwrap() {
        let src = entry.unwrap().path().join("src");
        if src.is_dir() { collect(&src, &root, &mut found); }
    }
    let manifest = serde_json::from_slice(
        &std::fs::read(root.join("docs/reasoner-capabilities.json")).unwrap()).unwrap();
    (found, manifest)
}

#[test]
fn inventory_accounts_for_every_production_preset_and_wrapper() {
    let (found, manifest) = scan_workspace();
    let found: BTreeSet<String> = found.into_keys().collect();
    let entries = manifest["presets"].as_array().unwrap();
    let mut declared = BTreeSet::new();
    for entry in entries {
        let callsite = entry["callsite"].as_str().unwrap();
        assert!(declared.insert(callsite.to_string()), "duplicate preset: {callsite}");
        for field in ["permission_profile", "output_contract"] {
            assert!(entry[field].as_str().is_some_and(|value| !value.is_empty()), "missing {field}: {callsite}");
        }
        assert!(entry["conformance"].as_array().is_some_and(|tests| !tests.is_empty()
            && tests.iter().all(|test| test.as_str().is_some_and(|name| !name.trim().is_empty()))),
            "missing conformance test: {callsite}");
        let profile = entry["permission_profile"].as_str().unwrap();
        assert!(manifest["profiles"][profile].is_object(), "unknown permission profile: {profile}");
    }
    assert_eq!(found, declared, "production presets changed; update the capability contract and conformance coverage");
}

/// #448/#1046: `model: None` emits no `--model`, so the spawned CLI inherits
/// the owner's interactive model and bills their subscription. Every
/// production construction must pin a model, and the manifest must document
/// the tier each callsite runs on, agreeing with any tier the source states.
#[test]
fn every_production_callsite_pins_its_documented_model_tier() {
    let (found, manifest) = scan_workspace();
    let unpinned: Vec<&String> = found.iter().filter(|(_, site)| site.unpinned).map(|(name, _)| name).collect();
    assert!(unpinned.is_empty(), "production callsites build ReasonerOpts with model: None and inherit \
        the owner's interactive model (#448); build them with ReasonerOpts::pinned(tier, ..): {unpinned:#?}");
    for entry in manifest["presets"].as_array().unwrap() {
        let callsite = entry["callsite"].as_str().unwrap();
        let tier = entry["model_tier"].as_str().unwrap_or_default();
        assert!(entry["tier_rationale"].as_str().is_some_and(|why| !why.trim().is_empty()),
            "missing tier_rationale: {callsite}");
        // A callsite missing from source is reported by the inventory test.
        let Some(site) = found.get(callsite) else { continue };
        if site.constructs {
            assert!(matches!(tier, "quality" | "fast"),
                "{callsite} builds ReasonerOpts: model_tier must be \"quality\" or \"fast\", got {tier:?}");
        } else {
            assert_eq!(tier, "preserved",
                "{callsite} wraps options built elsewhere: model_tier must be \"preserved\"");
        }
        for stated in &site.tiers {
            assert_eq!(stated, tier, "{callsite}: source pins {stated} but the manifest documents {tier}");
        }
    }
}

#[test]
fn core_presets_match_permission_contracts() {
    use augmentagent_channel_core::{reasoner::*, providers::{classify, tier_of, ModelTier, CapabilityClass::*}, codex_tools::BridgeLaunch};
    let fixture = tempfile::tempdir().unwrap();
    let wiki = fixture.path().join("wiki");
    std::fs::create_dir(&wiki).unwrap();
    let presets = vec![
        ("triage_opts", triage_opts(Some(wiki.clone())), ReadTools, false),
        ("draft_opts", draft_opts("Synthetic draft instructions".into(), Some(wiki.clone())), ReadTools, false),
        ("digest_opts", digest_opts(Some(wiki.clone())), ReadTools, false),
        ("lint_opts", lint_opts("Synthetic lint instructions".into(), wiki.clone()), ReadTools, false),
        ("wiki_migrate_opts", wiki_migrate_opts("Synthetic migration instructions".into(), wiki.clone()), ReadTools, false),
        // #1094: ingest ships a PreToolUse journal-guard hook; hooks are
        // honored only by the Claude CLI, so the preset must classify
        // FullAgentic (Claude-only) — a fallback provider would silently
        // bypass the guard.
        ("ingest_opts", ingest_opts("Synthetic ingestion instructions".into(), wiki.clone()), FullAgentic, true),
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
        // Presets whose model comes from a helper or env lookup are only
        // checkable at runtime; this keeps the documented tier honest.
        let tier = match tier_of(&opts) { ModelTier::Quality => "quality", ModelTier::Fast => "fast" };
        assert_eq!(entry["model_tier"], tier, "{name}: documented model tier drifted");
    }
    for opts in [triage_opts(None), draft_opts("Synthetic".into(), None), digest_opts(None)] {
        assert_eq!(classify(&opts), TextOnly, "optional wiki context must not grant tools when absent");
    }
}

/// Exercise the packaged MCP process with real production policies, rather than
/// only checking serialized allowlists. Model output contracts are separate.
#[test]
fn core_presets_execute_scoped_file_contracts_through_packaged_bridge() {
    use augmentagent_channel_core::{reasoner::*, codex_tools::BridgeLaunch};
    use serde_json::{json, Value};
    use std::io::Write;
    use std::process::{Command, Stdio};
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let wiki = fixture.path().join("wiki");
    std::fs::create_dir(&wiki).unwrap();
    let outside = fixture.path().join("outside.txt");
    std::fs::write(&outside, "SYNTHETIC_PRIVATE").unwrap();
    let presets = vec![
        ("triage", triage_opts(Some(wiki.clone()))),
        ("draft", draft_opts("Synthetic".into(), Some(wiki.clone()))),
        ("digest", digest_opts(Some(wiki.clone()))),
        ("lint", lint_opts("Synthetic".into(), wiki.clone())),
        ("migration", wiki_migrate_opts("Synthetic".into(), wiki.clone())),
        ("ingest", ingest_opts("Synthetic".into(), wiki.clone())),
        ("resume", resume_opts(wiki.clone())),
        ("ask", ask_opts(wiki.clone(), repo)),
        ("tone", tone_summarize_opts()),
        ("social", social_adapter_opts("Synthetic".into())),
        ("loop", loop_parse_opts()),
        ("archetype", archetype_pick_opts()),
    ];
    for (name, opts) in presets {
        std::fs::write(wiki.join("source.txt"), "SYNTHETIC_SOURCE\n").unwrap();
        let output_path = wiki.join(format!("{name}-result.txt"));
        let launch_dir = fixture.path().join(name);
        std::fs::create_dir(&launch_dir).unwrap();
        let launch = BridgeLaunch::prepare(&opts, &launch_dir).unwrap();
        let calls = [
            ("Read", json!({"file_path":wiki.join("source.txt")})),
            ("Glob", json!({"pattern":"source.txt", "path":wiki})),
            ("Grep", json!({"pattern":"SYNTHETIC_SOURCE", "path":wiki})),
            ("Write", json!({"file_path":output_path,"content":"SYNTHETIC_BEFORE\n"})),
            ("Edit", json!({"file_path":output_path,"old_string":"BEFORE","new_string":"AFTER"})),
            ("Read", json!({"file_path":outside})),
            ("Write", json!({"file_path":outside,"content":"UNAUTHORIZED"})),
        ];
        let mut child = Command::new("python3").arg(launch_dir.join("tool-bridge.py"))
            .arg(launch.policy_path).stdin(Stdio::piped()).stdout(Stdio::piped())
            .stderr(Stdio::piped()).spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        for (id, (tool, arguments)) in calls.iter().enumerate() {
            writeln!(input, "{}", json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                "params":{"name":tool,"arguments":arguments}})).unwrap();
        }
        drop(input);
        let result = child.wait_with_output().unwrap();
        assert!(result.status.success(), "{name}: bridge exited: {}", String::from_utf8_lossy(&result.stderr));
        let responses: Vec<Value> = String::from_utf8(result.stdout).unwrap().lines()
            .map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(responses.len(), calls.len(), "{name}: lost MCP responses");
        for (id, (tool, _)) in calls.iter().enumerate() {
            let permitted = id < 5 && opts.allowed_tools.iter().any(|allowed| allowed == tool);
            let response = &responses[id];
            let succeeded = response.get("error").is_none()
                && response["result"]["isError"] != true;
            assert_eq!(succeeded, permitted, "{name}: {tool} response {response}");
            if permitted && id < 3 {
                let expected = if id == 1 { "source.txt" } else { "SYNTHETIC_SOURCE" };
                assert!(response.to_string().contains(expected), "{name}: empty file-tool result");
            }
        }
        if opts.allowed_tools.iter().any(|tool| tool == "Write") {
            assert_eq!(std::fs::read_to_string(output_path).unwrap(), "SYNTHETIC_AFTER\n");
        } else {
            assert!(!output_path.exists(), "{name}: read-only policy wrote a file");
        }
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "SYNTHETIC_PRIVATE");
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
import json, runpy, sys, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
bridge = runpy.run_path(sys.argv[1])
config = json.load(open(sys.argv[2]))
received = []
class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        received.append((request, self.headers.get('Authorization')))
        if 'id' not in request:
            self.send_response(202); self.end_headers(); return
        if request['method'] == 'initialize':
            result = {'protocolVersion':'2025-03-26','capabilities':{'tools':{}},'serverInfo':{'name':'fixture','version':'1'}}
        elif request['method'] == 'tools/list':
            result = {'tools':[{'name':name,'inputSchema':{'type':'object','properties':{}}}
                for name in ['get_post','create_post']]}
        else:
            result = {'content':[{'type':'text','text':'SYNTHETIC_POST'}]}
        body = json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}).encode()
        self.send_response(200); self.send_header('Content-Type','application/json')
        self.send_header('Content-Length', str(len(body))); self.end_headers(); self.wfile.write(body)
httpd = ThreadingHTTPServer(('127.0.0.1',0), Handler)
threading.Thread(target=httpd.serve_forever,daemon=True).start()
config['settings']['mcpServers']['socialapi']['url'] = f'http://127.0.0.1:{httpd.server_port}/mcp'
policy = bridge['Policy'](config)
server = bridge['Server'](policy)
try:
    assert 'SYNTHETIC_POST' in str(server.call('mcp__socialapi__get_post', {}))
    try:
        server.call('mcp__socialapi__create_post', {})
    except bridge['Denied']:
        pass
    else:
        raise AssertionError('write operation escaped original read-only guard')
    calls = [request['params']['name'] for request, _ in received if request['method']=='tools/call']
    assert calls == ['get_post'], 'rejected mutation reached the remote endpoint'
    assert all(auth == 'Bearer fixture' for _, auth in received)
finally:
    server.close(); httpd.shutdown(); httpd.server_close()
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
    assert_eq!(inventory.presets.into_keys().collect::<BTreeSet<String>>(),
        BTreeSet::from(["resume_opts".into(), "optional".into(), "handle".into(), "mutate".into()]));
}

#[test]
fn scanner_reports_unpinned_models_and_stated_tiers() {
    let source = r#"
        #[cfg(test)] mod tests { fn fixture() -> ReasonerOpts { ReasonerOpts { model: None } } }
        fn inherits() { let opts = core::ReasonerOpts { system_prompt: s, model: None }; }
        fn clears(opts: &mut ReasonerOpts) { opts.model = None; }
        fn cheap() -> ReasonerOpts { ReasonerOpts { model: Some("claude-haiku-4-5".into()) } }
        fn helper() -> ReasonerOpts { ReasonerOpts { model: Some(opus_model()) } }
        impl Channel { async fn handle(&self) { let o = core::ReasonerOpts::pinned(core::ModelTier::Quality, p); } }
        fn dynamic(t: ModelTier) { let o = ReasonerOpts::pinned(t, p); }
        fn wraps(opts: ReasonerOpts) -> ReasonerOpts { opts }
    "#;
    let mut inventory = Inventory::default();
    inventory.visit_file(&syn::parse_file(source).unwrap());
    let sites = inventory.presets;
    assert!(!sites.contains_key("tests::fixture"), "test modules are not production callsites");
    assert!(sites["inherits"].unpinned && sites["inherits"].constructs);
    assert!(sites["clears"].unpinned);
    assert_eq!(sites["cheap"].tiers, BTreeSet::from(["fast".into()]));
    assert!(!sites["helper"].unpinned && sites["helper"].constructs && sites["helper"].tiers.is_empty());
    assert!(sites["handle"].constructs && !sites["handle"].unpinned);
    assert_eq!(sites["handle"].tiers, BTreeSet::from(["quality".into()]));
    assert_eq!(sites["dynamic"].tiers, BTreeSet::from(["unstated at the call site".into()]));
    assert!(!sites["wraps"].constructs && sites["wraps"].tiers.is_empty());
}

/// #1045 — re-run `test` in a child with the query preset's optional attachment
/// features configured (iMessage fetch and the transcript clone), so the process
/// environment it needs never races other tests. True in the parent.
fn rerun_with_query_attachment_features(test: &str) -> bool {
    use std::process::Command;
    const CHILD: &str = "JARVIS_QUERY_ATTACHMENT_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return false;
    }
    let scratch = tempfile::tempdir().unwrap();
    let transcripts = scratch.path().join("transcripts");
    std::fs::create_dir(&transcripts).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", test, "--nocapture", "--test-threads", "1"]);
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("AWS_") || name.starts_with("AUGMENTAGENT_") {
            command.env_remove(&key);
        }
    }
    let output = command
        .env(CHILD, "1")
        .env("AUGMENTAGENT_IMESSAGE_S3_BUCKET", "synthetic-bucket")
        .env("AUGMENTAGENT_TRANSCRIPTS_DIR", &transcripts)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}{}", String::from_utf8_lossy(&output.stderr));
    assert!(stdout.contains("1 passed"), "child did not run {test}: {stdout}");
    true
}

fn env_value(opts: &augmentagent_channel_core::reasoner::ReasonerOpts, key: &str) -> Option<String> {
    opts.env.iter().rev().find(|(name, _)| name == key).map(|(_, value)| value.clone())
}

/// #1045 red 1 — the query preset's bridge policy carries the scope guard's
/// single-file Read exceptions, including this session's iMessage attachment
/// dir, as pattern-scoped allowances. Nothing becomes a read root, and presets
/// without the guard get none.
#[test]
fn query_preset_policy_carries_attachment_read_allowances_not_wider_roots() {
    use augmentagent_channel_core::{reasoner::*, codex_tools::BridgeLaunch};
    use serde_json::{json, Value};
    if rerun_with_query_attachment_features("query_preset_policy_carries_attachment_read_allowances_not_wider_roots") {
        return;
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let wiki = fixture.path().join("wiki");
    std::fs::create_dir(&wiki).unwrap();
    let wiki = wiki.canonicalize().unwrap();
    let policy = |opts: &ReasonerOpts, name: &str| -> Value {
        let launch_dir = fixture.path().join(name);
        std::fs::create_dir(&launch_dir).unwrap();
        let launch = BridgeLaunch::prepare(opts, &launch_dir).unwrap();
        serde_json::from_slice(&std::fs::read(launch.policy_path).unwrap()).unwrap()
    };
    let discord = json!({"tools": ["Read"], "directory": "/tmp",
        "name_pattern": r"aa-(txt|img|doc)-[0-9]+-[0-9]+\.[a-zA-Z0-9]+"});
    // C3: the query preset's contract names this evidence.
    let manifest: Value = serde_json::from_slice(&std::fs::read(repo.join("docs/reasoner-capabilities.json")).unwrap()).unwrap();
    let entry = manifest["presets"].as_array().unwrap().iter()
        .find(|entry| entry["callsite"] == "crates/augmentagent-channel-core/src/reasoner.rs::ask_opts").unwrap();
    for test in ["capability_inventory::query_preset_policy_carries_attachment_read_allowances_not_wider_roots",
                 "capability_inventory::query_attachments_read_identically_under_claude_guard_and_codex_bridge"] {
        assert!(entry["conformance"].as_array().unwrap().iter().any(|name| name == test), "manifest lacks {test}");
    }

    let opts = ask_opts(wiki.clone(), repo.clone());
    let session = env_value(&opts, "AUGMENTAGENT_IMESSAGE_TMP_DIR").expect("session dir minted");
    let transcripts = PathBuf::from(env_value(&opts, "AUGMENTAGENT_TRANSCRIPTS_DIR").unwrap())
        .canonicalize().unwrap();
    let query = policy(&opts, "query");
    assert_eq!(query["read_allowances"], json!([discord.clone(),
        {"tools": ["Read"], "directory": session, "name_pattern": "[A-Za-z0-9._-]+"}]));
    assert_eq!(query["read_roots"], json!([wiki, transcripts]), "attachment dirs must not become read roots");
    assert_eq!(query["write_roots"], json!([wiki]));

    // Without iMessage fetch configured, only the Discord attachment exception remains.
    std::env::remove_var("AUGMENTAGENT_IMESSAGE_S3_BUCKET");
    let opts = ask_opts(wiki.clone(), repo);
    assert_eq!(env_value(&opts, "AUGMENTAGENT_IMESSAGE_TMP_DIR"), None);
    assert_eq!(policy(&opts, "query-no-imessage")["read_allowances"], json!([discord]));

    for (name, opts) in [
        ("triage", triage_opts(Some(wiki.clone()))),
        ("digest", digest_opts(Some(wiki.clone()))),
        ("lint", lint_opts("Synthetic".into(), wiki.clone())),
        ("ingest", ingest_opts("Synthetic".into(), wiki.clone())),
        ("resume", resume_opts(wiki.clone())),
        ("tone", tone_summarize_opts()),
    ] {
        assert_eq!(policy(&opts, name)["read_allowances"], json!([]), "{name}: no scope guard, no exceptions");
    }
}

struct QueryAttachmentFiles {
    files: Vec<PathBuf>,
    dirs: Vec<PathBuf>,
    /// The attachment root, when this test created it: removed only if empty.
    created_root: Option<PathBuf>,
}

impl Drop for QueryAttachmentFiles {
    fn drop(&mut self) {
        for file in &self.files {
            let _ = std::fs::remove_file(file);
        }
        for dir in &self.dirs {
            let _ = std::fs::remove_dir_all(dir);
        }
        if let Some(root) = &self.created_root {
            let _ = std::fs::remove_dir(root);
        }
    }
}

/// #1045 C1 — the same synthetic inbound attachments, read the way each provider
/// reads them. Claude: the real `aa-wiki-scope-guard.sh` hook decides, with the
/// query preset's environment. Codex: the packaged bridge with the query preset's
/// policy (which also runs that guard). Every probe must get the same decision
/// from both, and the expected one. Files live where the daemon writes them
/// (`/tmp`, `/tmp/aa-imsg/<session>`), with the mode its umask 0002 gives them
/// (0664), uniquely named and removed afterwards.
#[test]
fn query_attachments_read_identically_under_claude_guard_and_codex_bridge() {
    use augmentagent_channel_core::{reasoner::*, codex_tools::BridgeLaunch};
    use serde_json::{json, Value};
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    use std::process::{Command, Stdio};
    if rerun_with_query_attachment_features("query_attachments_read_identically_under_claude_guard_and_codex_bridge") {
        return;
    }
    if Command::new("jq").arg("--version").output().is_err() {
        eprintln!("skipping: the scope guard needs jq");
        return;
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let wiki = fixture.path().join("wiki");
    std::fs::create_dir_all(wiki.join("people")).unwrap();
    let opts = ask_opts(wiki.clone(), repo.clone());
    let wiki = wiki.canonicalize().unwrap();
    let session = PathBuf::from(env_value(&opts, "AUGMENTAGENT_IMESSAGE_TMP_DIR").expect("session dir minted"));
    let transcripts = PathBuf::from(env_value(&opts, "AUGMENTAGENT_TRANSCRIPTS_DIR").unwrap());

    // As the daemon writes them: its unit runs with UMask=0002.
    let daemon_file = |path: &Path, text: &str| {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)
            .unwrap_or_else(|error| panic!("create {}: {error}", path.display()));
        file.write_all(text.as_bytes()).unwrap();
        file.set_permissions(std::fs::Permissions::from_mode(0o664)).unwrap();
    };
    let unique = format!("{}{:09}", std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos());
    let discord_txt = PathBuf::from(format!("/tmp/aa-txt-{unique}-0.md"));
    let discord_doc = PathBuf::from(format!("/tmp/aa-doc-{unique}-1.txt"));
    let tmp_sibling = PathBuf::from(format!("/tmp/aa-note-{unique}.txt"));
    let other_session = PathBuf::from(format!("{}-other", session.display()));
    let mut cleanup = QueryAttachmentFiles { files: vec![], dirs: vec![], created_root: None };
    for (file, text) in [(&discord_txt, "SYNTHETIC_DISCORD_TEXT\n"), (&discord_doc, "SYNTHETIC_DISCORD_DOCUMENT\n"),
                         (&tmp_sibling, "SYNTHETIC_OUTSIDE_SCOPE")] {
        cleanup.files.push(file.clone());
        daemon_file(file, text);
    }
    let mut dirs = std::fs::DirBuilder::new();
    dirs.mode(0o700);
    match dirs.create(session.parent().unwrap()) {
        Ok(()) => cleanup.created_root = Some(session.parent().unwrap().to_path_buf()),
        Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => panic!("create attachment root: {error}"),
        Err(_) => {}
    }
    for dir in [&session, &other_session] {
        dirs.create(dir).unwrap();
        cleanup.dirs.push(dir.clone());
    }
    let imessage = session.join("9-note-3fa2b1c0.txt");
    daemon_file(&imessage, "SYNTHETIC_IMESSAGE_ATTACHMENT\n");
    daemon_file(&other_session.join("9-note-3fa2b1c0.txt"), "SYNTHETIC_OUTSIDE_SCOPE");
    std::fs::create_dir(session.join("nested")).unwrap();
    daemon_file(&session.join("nested/9-note.txt"), "SYNTHETIC_OUTSIDE_SCOPE");
    daemon_file(&wiki.join("people/sample.md"), "SYNTHETIC_WIKI_PAGE\n");
    daemon_file(&transcripts.join("meeting.md"), "SYNTHETIC_TRANSCRIPT\n");
    let outside = fixture.path().join("outside.txt");
    daemon_file(&outside, "SYNTHETIC_OUTSIDE_SCOPE");

    let path = |p: &Path| p.display().to_string();
    // (label, tool, arguments, readable, expected text)
    let probes: Vec<(&str, &str, Value, bool, &str)> = vec![
        ("wiki page", "Read", json!({"file_path": path(&wiki.join("people/sample.md"))}), true, "SYNTHETIC_WIKI_PAGE"),
        ("Discord text attachment", "Read", json!({"file_path": path(&discord_txt)}), true, "SYNTHETIC_DISCORD_TEXT"),
        ("Discord converted document", "Read", json!({"file_path": path(&discord_doc)}), true, "SYNTHETIC_DISCORD_DOCUMENT"),
        ("fetched iMessage attachment", "Read", json!({"file_path": path(&imessage)}), true, "SYNTHETIC_IMESSAGE_ATTACHMENT"),
        ("transcript read", "Read", json!({"file_path": path(&transcripts.join("meeting.md"))}), true, "SYNTHETIC_TRANSCRIPT"),
        ("transcript grep", "Grep", json!({"pattern": "SYNTHETIC_TRANSCRIPT", "path": path(&transcripts)}), true, "meeting.md"),
        ("transcript glob", "Glob", json!({"pattern": "*.md", "path": path(&transcripts)}), true, "meeting.md"),
        ("literal aa-txt-.. sibling", "Read", json!({"file_path": "/tmp/aa-txt-.."}), false, ""),
        ("dot-dot through an attachment name", "Read",
            json!({"file_path": format!("{}/../{}", path(&discord_txt), tmp_sibling.file_name().unwrap().to_str().unwrap())}), false, ""),
        ("non-attachment file in /tmp", "Read", json!({"file_path": path(&tmp_sibling)}), false, ""),
        ("attachment glob in /tmp", "Glob", json!({"pattern": "aa-txt-*", "path": "/tmp"}), false, ""),
        ("attachment grep", "Grep", json!({"pattern": "SYNTHETIC", "path": path(&discord_txt)}), false, ""),
        ("attachment write", "Write", json!({"file_path": path(&discord_txt), "content": "UNAUTHORIZED"}), false, ""),
        ("attachment edit", "Edit", json!({"file_path": path(&discord_txt), "old_string": "SYNTHETIC", "new_string": "UNAUTHORIZED"}), false, ""),
        ("another session's attachment", "Read", json!({"file_path": path(&other_session.join("9-note-3fa2b1c0.txt"))}), false, ""),
        ("escape from the session dir", "Read", json!({"file_path": format!("{}/../{}/9-note-3fa2b1c0.txt",
            path(&session), other_session.file_name().unwrap().to_str().unwrap())}), false, ""),
        ("nested path under the session dir", "Read", json!({"file_path": path(&session.join("nested/9-note.txt"))}), false, ""),
        ("session dir grep", "Grep", json!({"pattern": "SYNTHETIC", "path": path(&session)}), false, ""),
        ("session dir write", "Write", json!({"file_path": path(&session.join("new.txt")), "content": "UNAUTHORIZED"}), false, ""),
        ("transcript write", "Write", json!({"file_path": path(&transcripts.join("meeting.md")), "content": "UNAUTHORIZED"}), false, ""),
        ("outside file", "Read", json!({"file_path": path(&outside)}), false, ""),
    ];

    // Claude: the hook sees the query preset's environment (restrict_env keeps
    // only OS essentials plus `opts.env`) and runs in the preset's cwd.
    let guard_allows = |tool: &str, arguments: &Value| -> bool {
        let mut command = Command::new(repo.join("scripts/aa-wiki-scope-guard.sh"));
        command.env_clear();
        for (key, value) in std::env::vars() {
            if matches!(key.as_str(), "HOME" | "PATH" | "LANG") || key.starts_with("LC_") {
                command.env(key, value);
            }
        }
        let mut child = command.envs(opts.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .current_dir(opts.cwd.as_ref().unwrap())
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap()
            .write_all(json!({"tool_name": tool, "tool_input": arguments}).to_string().as_bytes()).unwrap();
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() {
            return false;
        }
        if stdout.trim().is_empty() {
            return true;
        }
        let decision: Value = serde_json::from_str(stdout.trim()).unwrap();
        !(decision["decision"] == "block" || decision["hookSpecificOutput"]["permissionDecision"] == "deny")
    };

    // Codex: one packaged bridge process with the query preset's policy.
    let launch_dir = fixture.path().join("launch");
    std::fs::create_dir(&launch_dir).unwrap();
    let launch = BridgeLaunch::prepare(&opts, &launch_dir).unwrap();
    let mut child = Command::new("python3").arg(launch_dir.join("tool-bridge.py")).arg(&launch.policy_path)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    for (id, (_, tool, arguments, _, _)) in probes.iter().enumerate() {
        writeln!(input, "{}", json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments}})).unwrap();
    }
    drop(input);
    let result = child.wait_with_output().unwrap();
    assert!(result.status.success(), "bridge exited: {}", String::from_utf8_lossy(&result.stderr));
    let responses: Vec<Value> = String::from_utf8(result.stdout).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(responses.len(), probes.len(), "lost MCP responses");

    let mut mismatches = Vec::new();
    for ((label, tool, arguments, readable, text), response) in probes.iter().zip(&responses) {
        let claude = guard_allows(tool, arguments);
        let codex = response.get("error").is_none() && response["result"]["isError"] != true;
        if claude != *readable || codex != *readable || (codex && !response.to_string().contains(text)) {
            mismatches.push(format!("{label}: expected {readable}, claude {claude}, codex {codex}: {response}"));
        }
    }
    assert!(mismatches.is_empty(), "providers disagree on attachment reads:\n{}", mismatches.join("\n"));
    assert_eq!(std::fs::read_to_string(&discord_txt).unwrap(), "SYNTHETIC_DISCORD_TEXT\n");
    assert!(!session.join("new.txt").exists());
    assert_eq!(std::fs::read_to_string(transcripts.join("meeting.md")).unwrap(), "SYNTHETIC_TRANSCRIPT\n");
}
