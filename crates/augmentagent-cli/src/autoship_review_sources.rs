//! Literal source requests are incomplete-review data, never execution grants.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSourceRequest {
    pub path: String,
    pub criterion_ids: Vec<String>,
    pub question: String,
    pub selector: ReviewSourceSelector,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReviewSourceSelector {
    WholeFile,
    RustItem { name: String },
    Bytes { start_byte: usize, end_byte: usize },
}
impl ReviewSourceRequest {
    pub(super) fn validate_review_source_request(
        &self,
        criteria: &BTreeMap<String, String>,
    ) -> Result<()> {
        ensure!(
            !self.path.is_empty() && self.path.len() <= 4096,
            "invalid source path length"
        );
        ensure!(
            !self.question.trim().is_empty() && self.question.len() <= 4096,
            "invalid source question"
        );
        ensure!(
            !self.criterion_ids.is_empty()
                && self.criterion_ids.iter().collect::<BTreeSet<_>>().len()
                    == self.criterion_ids.len()
                && self
                    .criterion_ids
                    .iter()
                    .all(|id| criteria.contains_key(id)),
            "invalid source question criteria"
        );
        match &self.selector {
            ReviewSourceSelector::RustItem { name } => {
                syn::parse_str::<syn::Ident>(name)
                    .context("source item must be a literal Rust identifier")?;
            }
            ReviewSourceSelector::Bytes {
                start_byte,
                end_byte,
            } => ensure!(start_byte < end_byte, "invalid source byte interval"),
            ReviewSourceSelector::WholeFile => {}
        }
        Ok(())
    }
}
/// #2418: repair a requested Rust item name that is not a literal identifier
/// (a path, a call, generics, a keyword prefix or a macro bang) to the item's
/// identifier, or ask for the whole admitted file when none remains. The
/// path stays admission-checked and budget-bounded at resolution, and an
/// absent item already resolves to the whole file (#2386).
pub(super) fn repair_item_selector(selector: &mut ReviewSourceSelector) {
    let ReviewSourceSelector::RustItem { name } = selector else { return };
    if syn::parse_str::<syn::Ident>(name).is_ok() {
        return;
    }
    let mut text = name.trim();
    for keyword in ["pub(crate) ", "pub ", "async ", "unsafe ", "fn ", "struct ", "enum ", "trait ", "impl ",
        "mod ", "const ", "static ", "type ", "union ", "macro_rules! "] {
        if let Some(rest) = text.strip_prefix(keyword) {
            text = rest.trim_start();
        }
    }
    // A qualified path names its last segment; `<T as Trait>::m` names `m`.
    let path_end = text.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':' || c == '#'))
        .filter(|end| !text[..*end].is_empty())
        .unwrap_or(text.len());
    let candidate = if text.starts_with('<') {
        text.rsplit("::").next().unwrap_or("")
    } else {
        text[..path_end].rsplit("::").next().unwrap_or("")
    };
    let candidate: String = candidate.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '#').collect();
    *selector = match syn::parse_str::<syn::Ident>(&candidate) {
        Ok(_) if !candidate.is_empty() => ReviewSourceSelector::RustItem { name: candidate },
        _ => ReviewSourceSelector::WholeFile,
    };
}

/// Enforce exact enum wire fields even for the internally tagged unit variant.
pub(super) fn validate_selector_wire(value: &serde_json::Value) -> Result<()> {
    let selector = value
        .get("selector")
        .and_then(serde_json::Value::as_object)
        .context("source selector must be an object")?;
    let fields: &[&str] = match selector.get("kind").and_then(serde_json::Value::as_str) {
        Some("whole_file") => &["kind"],
        Some("rust_item") => &["kind", "name"],
        Some("bytes") => &["kind", "start_byte", "end_byte"],
        _ => bail!("unknown source selector kind"),
    };
    ensure!(
        selector.len() == fields.len() && fields.iter().all(|field| selector.contains_key(*field)),
        "source selector fields differ from its kind"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(value: serde_json::Value) -> Result<(ReviewPass, Vec<ReviewSourceRequest>)> {
        parse_modified(value, |_| {})
    }
    fn parse_modified(
        value: serde_json::Value,
        mutate: impl FnOnce(&mut serde_json::Value),
    ) -> Result<(ReviewPass, Vec<ReviewSourceRequest>)> {
        let binding = ReviewBinding {
            head: "a".repeat(40),
            base: "b".repeat(40),
            environment_id: "fixture".into(),
            contract_hash: "c".repeat(64),
            evidence_hash: "d".repeat(64),
        };
        let actor = Actor {
            provider: "codex".into(),
            backend: "native".into(),
            model: "fixture".into(),
            session_id: "fixture-observation".into(),
        };
        let coverage = CoverageReceipt {
            binding: binding.clone(),
            required_paths: BTreeSet::new(),
            supplied_files: BTreeMap::new(),
            prompt_hash: digest(b"fixture"),
        };
        let mut envelope = serde_json::json!({"binding":binding,"decision":"incomplete","findings":[],"missing_sources":[value]});
        // A null fixture supplies the full envelope for edge cases below.
        if envelope["missing_sources"][0].is_null() {
            envelope["missing_sources"] = serde_json::Value::Null;
        }
        mutate(&mut envelope);
        parse_review_pass_with_sources(
            &envelope.to_string(),
            &binding,
            actor,
            ReviewRole::Diff,
            coverage,
            &BTreeMap::from([("C1".into(), "Preserve cap".into())]),
        )
    }
    fn source(selector: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"path":"src/logic.rs","criterion_ids":["C1"],"question":"Does caller preserve the cap?","selector":selector})
    }
    #[test]
    fn all_exact_selector_shapes_are_accepted_as_incomplete_data() {
        for selector in [
            serde_json::json!({"kind":"whole_file"}),
            serde_json::json!({"kind":"rust_item","name":"r#cap"}),
            serde_json::json!({"kind":"bytes","start_byte":0,"end_byte":10}),
        ] {
            let (pass, requests) = parse(source(selector)).unwrap();
            assert!(!pass.completed);
            assert_eq!(requests.len(), 1);
        }
    }
    #[test]
    fn source_request_limits_and_incomplete_only_envelopes_are_exact() {
        let good = source(serde_json::json!({"kind":"whole_file"}));
        let (_, requests) = parse_modified(good.clone(), |e| {
            e["missing_sources"] = serde_json::json!(vec![good.clone(); 16])
        })
        .unwrap();
        assert_eq!(requests.len(), 16);
        assert!(parse_modified(good.clone(), |e| e["missing_sources"] =
            serde_json::json!(vec![good.clone(); 17]))
        .is_err());
        for decision in ["approve", "changes_requested"] {
            assert!(parse_modified(good.clone(), |e| e["decision"] = decision.into()).is_err());
        }
        assert!(parse_modified(good.clone(), |e| e["missing_paths"] =
            serde_json::json!(["src/logic.rs"]))
        .is_err());
        let mut boundary = good;
        boundary["question"] = "q".repeat(4096).into();
        boundary["path"] = "p".repeat(4096).into();
        assert!(parse(boundary).is_ok());
    }
    /// #2418 (PR #2378, head b260fb89, 15:28Z and 16:15Z): codex asked for a
    /// `rust_item` whose name was not a literal identifier, and the whole
    /// review failed with "source item must be a literal Rust identifier:
    /// unexpected token". Production does not retain the raw response; these
    /// are the name shapes that produce exactly that syn error.
    #[test]
    fn malformed_rust_item_names_are_repaired_not_rejected() {
        for (raw, repaired) in [
            ("OwnerAlerts::post_digest", Some("post_digest")),
            ("post_digest()", Some("post_digest")),
            ("AlertDayDigest<'a>", Some("AlertDayDigest")),
            ("crate::owner_alert_digest::AlertDayDigest", Some("AlertDayDigest")),
            ("fn post_digest(&self)", Some("post_digest")),
            ("impl OwnerAlerts", Some("OwnerAlerts")),
            ("foo.*", Some("foo")),
            ("declare_digest!", Some("declare_digest")),
            ("::", None),
            ("<T as Trait>::method", Some("method")),
            ("impl", None),
        ] {
            if matches!(raw, "OwnerAlerts::post_digest" | "post_digest()" | "AlertDayDigest<'a>") {
                let error = syn::parse_str::<syn::Ident>(raw).unwrap_err().to_string();
                assert_eq!(error, "unexpected token", "{raw}: fixture matches the production error");
            }
            let (pass, requests) = parse(source(serde_json::json!({"kind":"rust_item","name":raw})))
                .unwrap_or_else(|error| panic!("{raw} rejected the review: {error:#}"));
            assert!(!pass.completed);
            let expected = match repaired {
                Some(name) => ReviewSourceSelector::RustItem { name: name.into() },
                None => ReviewSourceSelector::WholeFile,
            };
            assert_eq!(requests[0].selector, expected, "{raw}");
            assert_eq!(requests[0].path, "src/logic.rs", "the admitted path is unchanged");
        }
    }
    #[test]
    fn strict_source_schema_refuses_ambiguity_unknown_fields_and_invalid_bounds() {
        for selector in [
            serde_json::json!({"kind":"whole_file","name":"ignored"}),
            serde_json::json!({"kind":"bytes","start_byte":3,"end_byte":3}),
            serde_json::json!({"kind":"bytes","start_byte":-1,"end_byte":4}),
            serde_json::json!({"kind":"bytes","start_byte":0.5,"end_byte":4}),
            serde_json::json!({"kind":"unknown"}),
        ] {
            assert!(parse(source(selector)).is_err());
        }
        let good = source(serde_json::json!({"kind":"whole_file"}));
        for (key, value) in [
            ("extra", serde_json::json!(true)),
            ("question", serde_json::json!("")),
            ("question", serde_json::json!("x".repeat(4097))),
            ("path", serde_json::json!("x".repeat(4097))),
            ("criterion_ids", serde_json::json!([])),
            ("criterion_ids", serde_json::json!(["C1", "C1"])),
            ("criterion_ids", serde_json::json!(["other"])),
        ] {
            let mut changed = good.clone();
            changed[key] = value;
            assert!(parse(changed).is_err(), "accepted invalid {key}");
        }
        assert!(parse(serde_json::Value::Null).is_err());
    }
}
