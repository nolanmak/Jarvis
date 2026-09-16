//! Source inventory: adding a production preset requires an explicit contract.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
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
