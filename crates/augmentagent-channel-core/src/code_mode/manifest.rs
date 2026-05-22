//! v1 Code-Mode tool manifest.
//!
//! The manifest is the single source of truth for what the LLM is allowed to
//! call from generated TypeScript. From it we derive:
//!
//! * a `.d.ts` block (via [`ToolManifest::to_dts`]) that is injected into the
//!   system prompt so the model writes against real type signatures, and
//! * a flat allowlist of dotted tool names (via
//!   [`ToolManifest::to_runner_manifest`]) consumed by the Deno runner to
//!   gate host-bridge dispatch.
//!
//! Both outputs come from the same `Vec<ToolDef>` so they cannot drift.

use std::collections::BTreeMap;

/// One leaf tool in the v1 surface.
///
/// `name` is the dotted path the model uses to invoke the tool
/// (e.g. `"db.recentEmailsFrom"` or `"draft"` for a top-level tool).
/// `ts_signature` is the right-hand side of the TypeScript property — the
/// function signature including return type. `jsdoc` is the body of the
/// JSDoc comment rendered above the leaf (without the `/**` / `*/` wrappers).
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub ts_signature: String,
    pub jsdoc: String,
}

impl ToolDef {
    fn new(name: &str, ts_signature: &str, jsdoc: &str) -> Self {
        Self {
            name: name.to_string(),
            ts_signature: ts_signature.to_string(),
            jsdoc: jsdoc.to_string(),
        }
    }
}

/// Collection of [`ToolDef`]s that make up a Code-Mode tool surface.
#[derive(Debug, Clone, Default)]
pub struct ToolManifest {
    pub tools: Vec<ToolDef>,
}

impl ToolManifest {
    /// Flat allowlist of dotted tool names, in declaration order.
    ///
    /// Used by the Deno runner to gate host-bridge calls: any dispatch whose
    /// dotted name is not in this list is rejected before reaching Rust.
    pub fn to_runner_manifest(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name.clone()).collect()
    }

    /// Render the manifest as a `declare const tools: { ... }` TypeScript
    /// declaration block.
    ///
    /// Dotted names are nested into TypeScript object structure (so
    /// `db.threadHistory` becomes `db: { threadHistory: ... }`). Each leaf is
    /// preceded by a JSDoc block built from `ToolDef::jsdoc`. The output is a
    /// single string ready to be concatenated into the system prompt.
    pub fn to_dts(&self) -> String {
        let mut root = Node::default();
        for tool in &self.tools {
            root.insert(&tool.name, tool);
        }
        let mut out = String::new();
        out.push_str("declare const tools: ");
        root.render(&mut out, 0);
        out.push_str(";\n");
        out
    }
}

/// Internal tree node used to nest dotted names into TS object syntax.
#[derive(Default)]
struct Node<'a> {
    /// Set when this node *is* a leaf tool (in addition to possibly having
    /// children, though v1 has no such overlap).
    leaf: Option<&'a ToolDef>,
    /// `BTreeMap` keeps property order deterministic in the rendered output;
    /// snapshot tests and humans both prefer that.
    children: BTreeMap<String, Node<'a>>,
}

impl<'a> Node<'a> {
    fn insert(&mut self, dotted: &str, tool: &'a ToolDef) {
        let mut parts = dotted.splitn(2, '.');
        let head = parts.next().expect("non-empty tool name");
        match parts.next() {
            None => {
                // Terminal segment — this node is the leaf.
                self.children.entry(head.to_string()).or_default().leaf = Some(tool);
            }
            Some(rest) => {
                self.children
                    .entry(head.to_string())
                    .or_default()
                    .insert(rest, tool);
            }
        }
    }

    fn render(&self, out: &mut String, indent: usize) {
        let pad = "  ".repeat(indent);
        let inner_pad = "  ".repeat(indent + 1);
        out.push_str("{\n");
        for (name, child) in &self.children {
            match (&child.leaf, child.children.is_empty()) {
                (Some(tool), true) => {
                    // Pure leaf: JSDoc + `name: signature;`
                    push_jsdoc(out, &inner_pad, &tool.jsdoc);
                    out.push_str(&inner_pad);
                    out.push_str(name);
                    out.push_str(": ");
                    out.push_str(&tool.ts_signature);
                    out.push_str(";\n");
                }
                (None, false) => {
                    // Pure namespace: `name: { ... };`
                    out.push_str(&inner_pad);
                    out.push_str(name);
                    out.push_str(": ");
                    child.render(out, indent + 1);
                    out.push_str(";\n");
                }
                (Some(_), false) => {
                    // Not used in v1, but keep the emitter total: render as a
                    // namespace and drop the leaf signature (would need a
                    // callable-namespace type to express this in TS).
                    out.push_str(&inner_pad);
                    out.push_str(name);
                    out.push_str(": ");
                    child.render(out, indent + 1);
                    out.push_str(";\n");
                }
                (None, true) => {
                    // Empty interior — shouldn't happen, but emit an empty
                    // object so the output stays parseable.
                    out.push_str(&inner_pad);
                    out.push_str(name);
                    out.push_str(": {};\n");
                }
            }
        }
        out.push_str(&pad);
        out.push('}');
    }
}

fn push_jsdoc(out: &mut String, pad: &str, body: &str) {
    out.push_str(pad);
    out.push_str("/**\n");
    for line in body.lines() {
        out.push_str(pad);
        if line.is_empty() {
            out.push_str(" *\n");
        } else {
            out.push_str(" * ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str(pad);
    out.push_str(" */\n");
}

/// The v1 Code-Mode tool surface.
///
/// Six tools total: five non-terminal accessors (wiki / db / calendar) and
/// one terminal action (`draft`). The order matches the spec in issue #49.
pub fn manifest_v1() -> ToolManifest {
    ToolManifest {
        tools: vec![
            ToolDef::new(
                "wiki.draftHint",
                "(email: EmailContext) => Promise<string>",
                "Look up archetype/wiki guidance for an inbound email.\n\
                 Returns a short hint string the model can fold into its draft.",
            ),
            ToolDef::new(
                "db.recentEmailsFrom",
                "(sender: string, days: number) => Promise<EmailSummary[]>",
                "Most recent emails from `sender` within the last `days` days.\n\
                 Useful for cadence checks before drafting a reply.",
            ),
            ToolDef::new(
                "db.threadHistory",
                "(threadId: string) => Promise<EmailSummary[]>",
                "Full message history for the given thread, oldest first.",
            ),
            ToolDef::new(
                "db.isProcessed",
                "(messageId: string) => Promise<boolean>",
                "Whether a given message id has already been handled by the agent.\n\
                 Use this to short-circuit re-processing on retries.",
            ),
            ToolDef::new(
                "calendar.busy",
                "(startIso: string, endIso: string) => Promise<CalEvent[]>",
                "Busy intervals between two ISO-8601 timestamps.\n\
                 Used to resolve scheduling asks before drafting.",
            ),
            ToolDef::new(
                "draft",
                "(channel: Channel, body: string, reason: string) => Promise<void>",
                "Terminal action: emit a draft for the given channel with the\n\
                 provided body and a short reason string. Calling `draft` ends\n\
                 the Code-Mode turn.",
            ),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_manifest_has_six_leaves() {
        let m = manifest_v1();
        assert_eq!(m.to_runner_manifest().len(), 6);
    }

    #[test]
    fn runner_manifest_matches_expected_v1_surface() {
        let m = manifest_v1();
        assert_eq!(
            m.to_runner_manifest(),
            vec![
                "wiki.draftHint".to_string(),
                "db.recentEmailsFrom".to_string(),
                "db.threadHistory".to_string(),
                "db.isProcessed".to_string(),
                "calendar.busy".to_string(),
                "draft".to_string(),
            ]
        );
    }

    #[test]
    fn every_runner_name_appears_as_leaf_line_in_dts() {
        let m = manifest_v1();
        let dts = m.to_dts();
        for dotted in m.to_runner_manifest() {
            // The leaf segment is what's emitted as `<leaf>:` in the TS block.
            let leaf = dotted.rsplit('.').next().unwrap();
            let needle = format!("{leaf}:");
            assert!(
                dts.contains(&needle),
                "expected leaf line `{needle}` for `{dotted}` in:\n{dts}"
            );
        }
    }

    #[test]
    fn dts_contains_header_and_root_namespaces() {
        let dts = manifest_v1().to_dts();
        assert!(dts.contains("declare const tools"), "missing header in:\n{dts}");
        assert!(dts.contains("wiki:"), "missing wiki namespace in:\n{dts}");
        assert!(dts.contains("db:"), "missing db namespace in:\n{dts}");
        assert!(
            dts.contains("calendar:"),
            "missing calendar namespace in:\n{dts}"
        );
    }

    #[test]
    fn dts_emits_jsdoc_above_each_leaf() {
        let dts = manifest_v1().to_dts();
        // One `/**` per leaf — six leaves total.
        let jsdoc_opens = dts.matches("/**").count();
        assert_eq!(jsdoc_opens, 6, "expected 6 JSDoc blocks, got {jsdoc_opens} in:\n{dts}");
    }

    #[test]
    fn dts_renders_draft_at_root_not_nested() {
        let dts = manifest_v1().to_dts();
        // `draft` is a top-level tool — its signature should sit at root.
        assert!(
            dts.contains("draft: (channel: Channel, body: string, reason: string) => Promise<void>;"),
            "draft signature not rendered at root in:\n{dts}"
        );
    }
}
