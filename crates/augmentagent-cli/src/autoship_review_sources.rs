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
    fn strict_source_schema_refuses_ambiguity_unknown_fields_and_invalid_bounds() {
        for selector in [
            serde_json::json!({"kind":"whole_file","name":"ignored"}),
            serde_json::json!({"kind":"rust_item","name":"foo.*"}),
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
