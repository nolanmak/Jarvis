//! Coherent exact source lookup at one immutable candidate, without model tools.
use super::*;
use std::ops::Range;
use syn::{spanned::Spanned, visit::Visit};

const ITEM_BYTES: usize = 8 * 1024 * 1024;

fn source_interval_from_key(key: &str, body: &str) -> Result<Range<usize>> {
    let (_, range) = key
        .rsplit_once("#bytes-")
        .context("source stream lacks byte provenance")?;
    let (start, end) = range.split_once('-').context("invalid source interval")?;
    let (start, end) = (start.parse::<usize>()?, end.parse::<usize>()?);
    anyhow::ensure!(
        end >= start && end - start == body.len(),
        "source interval differs from actual bytes"
    );
    Ok(start..end)
}
async fn collect_source_interval(
    root: &Path,
    plan: &caller_stream::Plan,
    wanted: Range<usize>,
    limit: usize,
) -> Result<String> {
    let (_, _, size) = plan.single_source_identity()?;
    anyhow::ensure!(
        wanted.start <= wanted.end && wanted.end <= size,
        "source interval outside immutable blob"
    );
    if wanted.end - wanted.start > limit {
        return Err(
            EvidenceBudget("requested source exceeds its working-set budget".into()).into(),
        );
    }
    let mut stream = plan.stream(root);
    let mut output = String::new();
    let mut consumed = 0usize;
    while let Some(batch) = stream.next_batch().await? {
        let mut ordered = batch
            .into_iter()
            .map(|(key, body)| source_interval_from_key(&key, &body).map(|span| (span, body)))
            .collect::<Result<Vec<_>>>()?;
        ordered.sort_by_key(|(span, _)| span.start);
        for (span, body) in ordered {
            anyhow::ensure!(
                span.start == consumed,
                "source stream has a gap or duplicate"
            );
            consumed = span.end;
            let start = span.start.max(wanted.start);
            let end = span.end.min(wanted.end);
            if start < end {
                let (a, b) = (start - span.start, end - span.start);
                anyhow::ensure!(
                    body.is_char_boundary(a) && body.is_char_boundary(b),
                    "source interval splits a UTF-8 character"
                );
                output.push_str(&body[a..b]);
            }
        }
    }
    anyhow::ensure!(
        consumed == size && output.len() == wanted.end - wanted.start,
        "immutable source read was incomplete"
    );
    Ok(output)
}

struct ItemRanges<'a> {
    name: &'a str,
    starts: Vec<usize>,
    bytes: usize,
    context: Vec<Range<usize>>,
    ranges: Vec<Range<usize>>,
}
impl ItemRanges<'_> {
    fn source_line_range(&self, start: usize, end: usize) -> Range<usize> {
        self.starts[start.saturating_sub(1).min(self.starts.len() - 1)]
            ..self.starts.get(end).copied().unwrap_or(self.bytes)
    }
    fn source_span_range(&self, span: proc_macro2::Span) -> Range<usize> {
        self.source_line_range(span.start().line, span.end().line)
    }
    fn source_header_range(
        &self,
        span: proc_macro2::Span,
        brace: proc_macro2::Span,
    ) -> Range<usize> {
        self.source_line_range(span.start().line, brace.end().line)
    }
    fn collect_requested_item(&mut self, name: &syn::Ident, span: proc_macro2::Span) {
        if name.to_string().trim_start_matches("r#") == self.name {
            self.ranges.push(self.source_span_range(span));
            self.ranges.extend(self.context.iter().cloned());
        }
    }
}
macro_rules! lookup_item {
    ($method:ident,$kind:ident) => {
        fn $method(&mut self, item: &'ast syn::$kind) {
            self.collect_requested_item(&item.ident, item.span());
            syn::visit::$method(self, item);
        }
    };
}
impl<'ast> Visit<'ast> for ItemRanges<'_> {
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        self.collect_requested_item(&item.sig.ident, item.span());
        syn::visit::visit_item_fn(self, item);
    }
    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        self.collect_requested_item(&item.sig.ident, item.span());
        syn::visit::visit_impl_item_fn(self, item);
    }
    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        self.collect_requested_item(&item.sig.ident, item.span());
        syn::visit::visit_trait_item_fn(self, item);
    }
    fn visit_foreign_item_fn(&mut self, item: &'ast syn::ForeignItemFn) {
        self.collect_requested_item(&item.sig.ident, item.span());
        syn::visit::visit_foreign_item_fn(self, item);
    }
    lookup_item!(visit_item_struct, ItemStruct);
    lookup_item!(visit_item_enum, ItemEnum);
    lookup_item!(visit_item_union, ItemUnion);
    lookup_item!(visit_item_trait_alias, ItemTraitAlias);
    lookup_item!(visit_item_const, ItemConst);
    lookup_item!(visit_item_static, ItemStatic);
    lookup_item!(visit_item_type, ItemType);
    lookup_item!(visit_impl_item_const, ImplItemConst);
    lookup_item!(visit_impl_item_type, ImplItemType);
    lookup_item!(visit_trait_item_const, TraitItemConst);
    lookup_item!(visit_trait_item_type, TraitItemType);
    lookup_item!(visit_foreign_item_static, ForeignItemStatic);
    lookup_item!(visit_foreign_item_type, ForeignItemType);
    fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
        if let Some(name) = &item.ident {
            self.collect_requested_item(name, item.span());
        }
        syn::visit::visit_item_macro(self, item);
    }
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        self.collect_requested_item(&item.ident, item.span());
        if let Some((brace, _)) = &item.content {
            self.context
                .push(self.source_header_range(item.span(), brace.span.open()));
            syn::visit::visit_item_mod(self, item);
            self.context.pop();
        }
    }
    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        self.context
            .push(self.source_header_range(item.span(), item.brace_token.span.open()));
        syn::visit::visit_item_impl(self, item);
        self.context.pop();
    }
    fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
        self.collect_requested_item(&item.ident, item.span());
        self.context
            .push(self.source_header_range(item.span(), item.brace_token.span.open()));
        syn::visit::visit_item_trait(self, item);
        self.context.pop();
    }
    fn visit_item_foreign_mod(&mut self, item: &'ast syn::ItemForeignMod) {
        self.context
            .push(self.source_header_range(item.span(), item.brace_token.span.open()));
        syn::visit::visit_item_foreign_mod(self, item);
        self.context.pop();
    }
}
fn requested_item_ranges(source: &str, name: &str) -> Result<Vec<Range<usize>>> {
    syn::parse_str::<syn::Ident>(name).context("source item is not a literal identifier")?;
    let file = syn::parse_file(source).context("requested Rust source cannot be parsed")?;
    let mut starts = vec![0];
    starts.extend(
        source
            .bytes()
            .enumerate()
            .filter_map(|(i, b)| (b == b'\n').then_some(i + 1)),
    );
    let mut lookup = ItemRanges {
        name: name.strip_prefix("r#").unwrap_or(name),
        starts,
        bytes: source.len(),
        context: vec![],
        ranges: vec![],
    };
    lookup.visit_file(&file);
    anyhow::ensure!(!lookup.ranges.is_empty(), "requested Rust item is absent");
    lookup.ranges.sort_by_key(|r| (r.start, r.end));
    let mut ranges: Vec<Range<usize>> = vec![];
    for range in lookup.ranges {
        if let Some(last) = ranges.last_mut() {
            if range.start < last.end {
                last.end = last.end.max(range.end);
                continue;
            }
        }
        ranges.push(range);
    }
    Ok(ranges)
}

pub(super) async fn resolve_review_source_request(
    root: &Path,
    head: &str,
    request: &aligned::ReviewSourceRequest,
) -> Result<BTreeMap<String, String>> {
    anyhow::ensure!(
        requested_evidence::admitted_path(&request.path),
        "requested source path is not admitted"
    );
    if let aligned::ReviewSourceSelector::Bytes {
        start_byte,
        end_byte,
    } = &request.selector
    {
        let (start_byte, end_byte) = (*start_byte, *end_byte);
        anyhow::ensure!(start_byte < end_byte, "invalid requested source interval");
        if end_byte - start_byte > aligned::MAX_PROMPT_BYTES {
            return Err(EvidenceBudget("requested byte range exceeds review budget".into()).into());
        }
    }
    let plan = caller_stream::Plan::named(root, head, &request.path).await?;
    let (_, _, size) = plan.single_source_identity()?;
    let (source, ranges) = match &request.selector {
        aligned::ReviewSourceSelector::WholeFile => {
            let body =
                collect_source_interval(root, &plan, 0..size, aligned::MAX_PROMPT_BYTES).await?;
            (body, vec![0..size])
        }
        aligned::ReviewSourceSelector::RustItem { name } => {
            let body = collect_source_interval(root, &plan, 0..size, ITEM_BYTES).await?;
            let ranges = requested_item_ranges(&body, name)?;
            (body, ranges)
        }
        aligned::ReviewSourceSelector::Bytes {
            start_byte,
            end_byte,
        } => {
            let body = collect_source_interval(
                root,
                &plan,
                *start_byte..*end_byte,
                aligned::MAX_PROMPT_BYTES,
            )
            .await?;
            let key = format!(
                "__requested_source__:{}#bytes-{start_byte}-{end_byte}",
                serde_json::to_string(&request.path)?
            );
            return Ok(BTreeMap::from([(key, body)]));
        }
    };
    let bytes = ranges.iter().try_fold(0usize, |sum, r| {
        sum.checked_add(r.end - r.start)
            .context("source range sum overflow")
    })?;
    if bytes > aligned::MAX_PROMPT_BYTES {
        return Err(EvidenceBudget(
            "all matching source items and companions exceed review budget".into(),
        )
        .into());
    }
    let mut result = BTreeMap::new();
    for range in ranges {
        anyhow::ensure!(
            range.end <= source.len()
                && source.is_char_boundary(range.start)
                && source.is_char_boundary(range.end),
            "invalid parsed item range"
        );
        let key = format!(
            "__requested_source__:{}#bytes-{}-{}",
            serde_json::to_string(&request.path)?,
            range.start,
            range.end
        );
        result.insert(key, source[range].into());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn committed_fixture() -> (tempfile::TempDir, String) {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("logic.rs"),
            "// αβ\nfn cap() -> u32 { 3 }\n",
        )
        .unwrap();
        std::os::unix::fs::symlink("logic.rs", root.path().join("link.rs")).unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["add", "."],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
        ] {
            assert!(std::process::Command::new("git")
                .args(args)
                .current_dir(root.path())
                .output()
                .unwrap()
                .status
                .success());
        }
        let head = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root.path())
            .output()
            .unwrap();
        (root, String::from_utf8(head.stdout).unwrap().trim().into())
    }
    #[tokio::test]
    async fn immutable_byte_windows_reject_symlinks_and_non_utf8_boundaries() {
        let (root, head) = committed_fixture();
        std::fs::write(root.path().join("logic.rs"), "uncommitted poison").unwrap();
        let mut request = aligned::ReviewSourceRequest {
            path: "logic.rs".into(),
            criterion_ids: vec!["C1".into()],
            question: "Inspect cap".into(),
            selector: aligned::ReviewSourceSelector::Bytes {
                start_byte: 3,
                end_byte: 7,
            },
        };
        let files = resolve_review_source_request(root.path(), &head, &request)
            .await
            .unwrap();
        assert_eq!(files.values().next().unwrap(), "αβ");
        for (start, end) in [(4, 7), (3, 6), (0, 999)] {
            request.selector = aligned::ReviewSourceSelector::Bytes {
                start_byte: start,
                end_byte: end,
            };
            assert!(resolve_review_source_request(root.path(), &head, &request)
                .await
                .is_err());
        }
        request.path = "link.rs".into();
        request.selector = aligned::ReviewSourceSelector::WholeFile;
        assert!(resolve_review_source_request(root.path(), &head, &request)
            .await
            .is_err());
        request.path = "logic.rs".into();
        request.selector = aligned::ReviewSourceSelector::RustItem { name: "cap".into() };
        let files = resolve_review_source_request(root.path(), &head, &request)
            .await
            .unwrap();
        assert!(files
            .values()
            .any(|body| body.contains("fn cap() -> u32 { 3 }")));
        assert!(!files.values().any(|body| body.contains("poison")));
    }
    #[test]
    fn raw_names_duplicates_and_container_attributes_are_exact_source() {
        let source="#[cfg(any())]\nmod first {\nstruct Foo;\nimpl Foo\nwhere Foo: Sized\n{\n#[inline]\nfn r#cap() -> u32 { 3 }\n}\n}\nmod second {\nfn cap() -> u32 { 4 }\n}\n";
        for name in ["cap", "r#cap"] {
            let ranges = requested_item_ranges(source, name).unwrap();
            let text = ranges
                .into_iter()
                .map(|r| &source[r])
                .collect::<Vec<_>>()
                .join("");
            for required in [
                "#[cfg(any())]",
                "mod first",
                "impl Foo",
                "where Foo: Sized",
                "#[inline]",
                "fn r#cap()",
                "mod second",
                "fn cap()",
            ] {
                assert!(text.contains(required), "missing {required}");
            }
        }
        assert!(requested_item_ranges(source, "Cap").is_err());
        assert!(requested_item_ranges("invoke! { fn hidden() {} }", "hidden").is_err());
        assert!(requested_item_ranges("struct Bad {", "Bad").is_err());
    }
    #[tokio::test]
    async fn byte_window_can_cross_large_blob_batches_but_cannot_hide_invalid_eof() {
        let (root, _) = committed_fixture();
        let body = format!("{}αβtail", "x".repeat(ITEM_BYTES + 4096));
        std::fs::write(root.path().join("logic.rs"), &body).unwrap();
        let commit = |message: &str| {
            for args in [
                vec!["add", "logic.rs"],
                vec![
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "commit",
                    "-qm",
                    message,
                ],
            ] {
                assert!(std::process::Command::new("git")
                    .args(args)
                    .current_dir(root.path())
                    .output()
                    .unwrap()
                    .status
                    .success());
            }
            let out = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(root.path())
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        let head = commit("large source");
        let start = ITEM_BYTES - 64 * 1024;
        let mut request = aligned::ReviewSourceRequest {
            path: "logic.rs".into(),
            criterion_ids: vec!["C1".into()],
            question: "Inspect exact boundary".into(),
            selector: aligned::ReviewSourceSelector::Bytes {
                start_byte: start,
                end_byte: body.len(),
            },
        };
        let files = resolve_review_source_request(root.path(), &head, &request)
            .await
            .unwrap();
        assert_eq!(files.values().next().unwrap(), &body[start..]);
        request.selector = aligned::ReviewSourceSelector::RustItem { name: "cap".into() };
        let error = resolve_review_source_request(root.path(), &head, &request)
            .await
            .unwrap_err();
        assert!(is_evidence_budget(&error));
        let mut invalid = body.into_bytes();
        invalid.push(0xff);
        std::fs::write(root.path().join("logic.rs"), invalid).unwrap();
        let head = commit("invalid suffix");
        request.selector = aligned::ReviewSourceSelector::Bytes {
            start_byte: 0,
            end_byte: 3,
        };
        assert!(
            resolve_review_source_request(root.path(), &head, &request)
                .await
                .is_err(),
            "a valid selected prefix cannot publish before the entire immutable stream validates"
        );
    }
}
