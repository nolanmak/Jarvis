//! Identify changed Rust declarations from immutable source and changed lines.
use super::*;
use std::ops::RangeInclusive;
use syn::{spanned::Spanned, visit::Visit};

pub(super) fn names(source: &str, edits: &[RangeInclusive<usize>]) -> Result<BTreeSet<String>> {
    Ok(scoped_names(source, edits)?.0)
}
/// Changed item names, and the subset that is only ever declared crate-local
/// (private, `pub(crate)`, `pub(super)`, `pub(in ..)`, outside trait impls).
/// Rust forbids naming those from another crate, so their callers are found
/// within the declaring crate; a generic private name such as `first` then no
/// longer drags every file of the workspace into review evidence (#1888).
pub(super) fn scoped_names(source: &str, edits: &[RangeInclusive<usize>]) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    let file = syn::parse_file(source).context("cannot identify changed Rust items")?;
    let mut visitor = Changed {
        edits,
        names: BTreeSet::new(),
        public: BTreeSet::new(),
        in_trait_impl: false,
    };
    visitor.visit_file(&file);
    let local = visitor.names.difference(&visitor.public).cloned().collect();
    Ok((visitor.names, local))
}
struct Changed<'a> {
    edits: &'a [RangeInclusive<usize>],
    names: BTreeSet<String>,
    /// Names with at least one changed declaration reachable outside its crate.
    public: BTreeSet<String>,
    in_trait_impl: bool,
}
impl Changed<'_> {
    fn intersects(&self, span: proc_macro2::Span) -> bool {
        let (start, end) = (span.start().line, span.end().line);
        self.edits
            .iter()
            .any(|edit| *edit.start() <= end && *edit.end() >= start)
    }
    fn add(&mut self, name: &syn::Ident, span: proc_macro2::Span) {
        self.add_vis(name, span, None);
    }
    /// `vis` is the declaration's visibility; `None` means it cannot be shown
    /// crate-local (macros, trait members, impl headers), so it stays public.
    fn add_vis(&mut self, name: &syn::Ident, span: proc_macro2::Span, vis: Option<&syn::Visibility>) {
        if self.intersects(span) {
            let name: String = name.to_string().trim_start_matches("r#").into();
            let local = !self.in_trait_impl && vis.is_some_and(|v| !matches!(v, syn::Visibility::Public(_)));
            if !local { self.public.insert(name.clone()); }
            self.names.insert(name);
        }
    }
}
macro_rules! named_item {
    ($method:ident,$kind:ident) => {
        fn $method(&mut self, item: &'ast syn::$kind) {
            self.add(&item.ident, item.span());
            syn::visit::$method(self, item);
        }
    };
}
macro_rules! visible_item {
    ($method:ident,$kind:ident) => {
        fn $method(&mut self, item: &'ast syn::$kind) {
            self.add_vis(&item.ident, item.span(), Some(&item.vis));
            syn::visit::$method(self, item);
        }
    };
}
impl<'ast> Visit<'ast> for Changed<'_> {
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        self.add_vis(&item.sig.ident, item.span(), Some(&item.vis));
        syn::visit::visit_item_fn(self, item);
    }
    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        self.add_vis(&item.sig.ident, item.span(), Some(&item.vis));
        syn::visit::visit_impl_item_fn(self, item);
    }
    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        self.add(&item.sig.ident, item.span());
        syn::visit::visit_trait_item_fn(self, item);
    }
    fn visit_foreign_item_fn(&mut self, item: &'ast syn::ForeignItemFn) {
        self.add(&item.sig.ident, item.span());
        syn::visit::visit_foreign_item_fn(self, item);
    }
    visible_item!(visit_item_struct, ItemStruct);
    visible_item!(visit_item_enum, ItemEnum);
    visible_item!(visit_item_union, ItemUnion);
    named_item!(visit_item_trait, ItemTrait);
    named_item!(visit_item_trait_alias, ItemTraitAlias);
    visible_item!(visit_item_const, ItemConst);
    visible_item!(visit_item_static, ItemStatic);
    visible_item!(visit_item_type, ItemType);
    named_item!(visit_impl_item_const, ImplItemConst);
    named_item!(visit_impl_item_type, ImplItemType);
    named_item!(visit_trait_item_const, TraitItemConst);
    named_item!(visit_trait_item_type, TraitItemType);
    named_item!(visit_foreign_item_static, ForeignItemStatic);
    named_item!(visit_foreign_item_type, ForeignItemType);
    fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
        let name = item
            .ident
            .as_ref()
            .or_else(|| item.mac.path.segments.last().map(|part| &part.ident));
        if let Some(name) = name {
            self.add(name, item.span());
        }
        syn::visit::visit_item_macro(self, item);
    }
    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        // An impl-header change can alter every associated method even when
        // none of their bodies changed. A body edit selects only its own item.
        let start = item.span().start().line;
        let end = item.brace_token.span.open().end().line;
        if self
            .edits
            .iter()
            .any(|range| *range.start() <= end && *range.end() >= start)
        {
            if let Some((_, path, _)) = &item.trait_ {
                if let Some(part) = path.segments.last() {
                    let name: String = part.ident.to_string().trim_start_matches("r#").into();
                    self.public.insert(name.clone());
                    self.names.insert(name);
                }
            }
            let mut target = item.self_ty.as_ref();
            loop {
                match target {
                    syn::Type::Reference(value) => target = &value.elem,
                    syn::Type::Paren(value) => target = &value.elem,
                    syn::Type::Group(value) => target = &value.elem,
                    _ => break,
                }
            }
            if let syn::Type::Path(value) = target {
                if let Some(part) = value.path.segments.last() {
                    let generic = item.generics.params.iter().any(|param| match param {
                        syn::GenericParam::Type(value) => value.ident == part.ident,
                        syn::GenericParam::Const(value) => value.ident == part.ident,
                        syn::GenericParam::Lifetime(_) => false,
                    });
                    if !generic {
                        let name: String = part.ident.to_string().trim_start_matches("r#").into();
                        self.public.insert(name.clone());
                        self.names.insert(name);
                    }
                }
            }
            for member in &item.items {
                let ident = match member {
                    syn::ImplItem::Fn(value) => Some(&value.sig.ident),
                    syn::ImplItem::Const(value) => Some(&value.ident),
                    syn::ImplItem::Type(value) => Some(&value.ident),
                    _ => None,
                };
                if let Some(ident) = ident {
                    let name: String = ident.to_string().trim_start_matches("r#").into();
                    // Header changes affect every member; stay conservative.
                    self.public.insert(name.clone());
                    self.names.insert(name);
                }
            }
        }
        let outer = std::mem::replace(&mut self.in_trait_impl, item.trait_.is_some());
        syn::visit::visit_item_impl(self, item);
        self.in_trait_impl = outer;
    }
}

fn ranges(diff: &str) -> Result<(Vec<RangeInclusive<usize>>, Vec<RangeInclusive<usize>>)> {
    fn range(text: &str, prefix: char) -> Result<Option<RangeInclusive<usize>>> {
        let text = text
            .strip_prefix(prefix)
            .context("invalid caller hunk range")?;
        let (start, count) = text.split_once(',').unwrap_or((text, "1"));
        let (start, count) = (start.parse::<usize>()?, count.parse::<usize>()?);
        if count == 0 {
            return Ok(None);
        }
        anyhow::ensure!(start > 0, "invalid caller hunk start");
        Ok(Some(
            start
                ..=start
                    .checked_add(count - 1)
                    .context("caller hunk range overflow")?,
        ))
    }
    let (mut before, mut after) = (Vec::new(), Vec::new());
    for line in diff.lines().filter(|line| line.starts_with("@@ ")) {
        let parts: Vec<_> = line.split_whitespace().take(4).collect();
        anyhow::ensure!(
            parts.len() == 4 && parts[0] == "@@" && parts[3] == "@@",
            "invalid caller diff hunk"
        );
        if let Some(range) = range(parts[1], '-')? {
            before.push(range);
        }
        if let Some(range) = range(parts[2], '+')? {
            after.push(range);
        }
    }
    Ok((before, after))
}

async fn source(root: &Path, head: &str, path: &str) -> Result<Option<String>> {
    let tree = git(
        root,
        &["--literal-pathspecs", "ls-tree", "-z", head, "--", path],
    )
    .await?;
    if tree.is_empty() {
        return Ok(None);
    }
    let entries: Vec<_> = tree.split('\0').filter(|line| !line.is_empty()).collect();
    anyhow::ensure!(entries.len() == 1, "caller source is not one tracked file");
    let (metadata, name) = entries[0]
        .split_once('\t')
        .context("caller source path missing")?;
    let parts: Vec<_> = metadata.split_whitespace().collect();
    anyhow::ensure!(
        name == path
            && parts.len() == 3
            && matches!(parts[0], "100644" | "100755")
            && parts[1] == "blob",
        "caller source must be a regular immutable file"
    );
    Ok(Some(git(root, &["cat-file", "blob", parts[2]]).await?))
}

/// `crates/<name>/` for a path inside a workspace crate.
fn crate_dir(path: &str) -> Option<String> {
    let mut parts = path.splitn(3, '/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("crates"), Some(name), Some(_)) if !name.is_empty() => Some(format!("crates/{name}/")),
        _ => None,
    }
}
/// A symbol whose callers are searched only within `dir`. Never reaches git
/// as a pattern; see [`split_symbol`].
pub(super) fn scoped_symbol(name: &str, dir: &str) -> String {
    format!("{name}\u{0}{dir}")
}
/// `(pattern, Some(crate dir))` for a crate-local symbol, `(pattern, None)` otherwise.
pub(super) fn split_symbol(symbol: &str) -> (&str, Option<&str>) {
    match symbol.split_once('\u{0}') { Some((name, dir)) => (name, Some(dir)), None => (symbol, None) }
}

pub(super) async fn collect(
    root: &Path,
    base: &str,
    head: &str,
    paths: &[&String],
) -> Result<BTreeSet<String>> {
    let mut symbols = BTreeSet::new();
    for path in paths {
        let diff = git(
            root,
            &[
                "--literal-pathspecs",
                "diff",
                "--no-renames",
                "--unified=0",
                base,
                head,
                "--",
                path,
            ],
        )
        .await?;
        if !path.ends_with(".rs") {
            // Preserve the legacy lexical lookup for non-Rust artifacts.
            // Only Rust source can supply syntax-aware enclosing declarations.
            let removed = diff
                .lines()
                .filter_map(|line| line.strip_prefix('-').map(|text| format!("+{text}")))
                .collect::<Vec<_>>()
                .join("\n");
            symbols.extend(changed_symbols(&diff));
            symbols.extend(changed_symbols(&removed));
            continue;
        }
        let (old_edits, new_edits) = ranges(&diff)?;
        for (revision, mut edits) in [(base, old_edits), (head, new_edits)] {
            let Some(source) = source(root, revision, path).await? else {
                continue;
            };
            if diff.contains("GIT binary patch") || diff.contains("Binary files ") {
                edits = vec![1..=source.lines().count().max(1)];
            }
            if edits.is_empty() {
                continue;
            }
            let (all, local) = scoped_names(&source, &edits).map_err(|error| {
                EvidenceBudget(format!(
                    "cannot identify edited Rust items in {path}: {error:#}"
                ))
            })?;
            let scope = crate_dir(path);
            for name in all {
                match &scope {
                    Some(dir) if local.contains(&name) => { symbols.insert(scoped_symbol(&name, dir)); }
                    _ => { symbols.insert(name); }
                }
            }
        }
    }
    Ok(symbols)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn body_edit_selects_its_item_without_neighboring_module_functions_or_string_items() {
        let source="mod tests {\n    fn store() {}\n    pub fn resolve_person() {\n        let text = r#\"fn main() {}\"#;\n        consume(text);\n    }\n    fn wiki() {}\n}\n";
        assert_eq!(
            names(source, &[4..=5]).unwrap(),
            BTreeSet::from(["resolve_person".into()])
        );
    }
    #[test]
    fn private_items_are_crate_local_and_public_or_trait_members_are_not() {
        let source="enum Handles { One }\nimpl Handles {\n    fn first(self) -> u8 {\n        1\n    }\n}\npub fn close_contacts() -> u8 {\n    2\n}\nimpl Clone for Handles {\n    fn clone(&self) -> Self {\n        Handles::One\n    }\n}\npub(crate) fn helper() {}\n";
        let (all, local) = scoped_names(source, &[4..=4, 8..=8, 12..=12, 15..=15]).unwrap();
        assert_eq!(all, BTreeSet::from(["first".into(), "close_contacts".into(), "clone".into(), "helper".into()]));
        assert_eq!(local, BTreeSet::from(["first".into(), "helper".into()]),
            "private and pub(crate) are local; pub and trait-impl members are not");
        assert_eq!(crate_dir("crates/augmentagent-channel-instagram/src/feed.rs").as_deref(), Some("crates/augmentagent-channel-instagram/"));
        assert_eq!(crate_dir("scripts/x.rs"), None);
        let token = scoped_symbol("first", "crates/a/");
        assert_eq!(split_symbol(&token), ("first", Some("crates/a/")));
        assert_eq!(split_symbol("close_contacts"), ("close_contacts", None));
    }
    #[test]
    fn syntax_failure_cannot_be_treated_as_an_empty_caller_inventory() {
        assert!(names("pub fn broken( {", &[1..=1]).is_err());
    }

    #[test]
    fn method_body_and_impl_header_have_distinct_affected_items() {
        let source="struct Person;\nimpl Display for Person {\n    fn untouched(&self) {}\n    fn resolve(&self) -> u32 {\n        1\n    }\n    fn other(&self) {}\n}\n";
        assert_eq!(
            names(source, &[5..=5]).unwrap(),
            BTreeSet::from(["resolve".into()])
        );
        assert_eq!(
            names(source, &[2..=2]).unwrap(),
            BTreeSet::from([
                "Person".into(),
                "Display".into(),
                "untouched".into(),
                "resolve".into(),
                "other".into()
            ])
        );
    }

    #[test]
    fn hunk_ranges_keep_deletions_and_insertions_on_the_correct_revision() {
        let (old, new) = ranges("@@ -3,0 +4,2 @@\n@@ -8,2 +10,0 @@\n@@ -12 +13 @@\n").unwrap();
        assert_eq!(old, vec![8..=9, 12..=12]);
        assert_eq!(new, vec![4..=5, 13..=13]);
        assert!(ranges("@@ -0,2 +1 @@").is_err());
        assert!(ranges("@@ -1 +nope @@").is_err());
        assert!(ranges(&format!("@@ -{},2 +1 @@", usize::MAX)).is_err());
    }
}
