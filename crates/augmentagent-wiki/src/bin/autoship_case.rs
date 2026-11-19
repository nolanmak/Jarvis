use std::collections::BTreeMap;
use std::fs;
use std::sync::{Arc, Mutex};

use augmentagent_wiki::{IdentityIndex, WikiLayout};
use tracing::field::{Field, Visit};
use tracing::metadata::LevelFilter;
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::Interest;
use tracing::{Event, Metadata, Subscriber};

struct WarningSubscriber {
    events: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
}

struct FieldVisitor(BTreeMap<String, String>);

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        self.0.insert(
            field.name().to_string(),
            rendered.trim_matches('\"').to_string(),
        );
    }
}

impl Subscriber for WarningSubscriber {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
        Interest::always()
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::TRACE)
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut visitor = FieldVisitor(BTreeMap::new());
        event.record(&mut visitor);
        self.events.lock().unwrap().push(visitor.0);
    }

    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

fn write_page(layout: &WikiLayout, slug: &str, front_matter: &str) {
    fs::write(
        layout.people_dir().join(format!("{slug}.md")),
        format!("---\n{front_matter}\n---\n# {slug}\n"),
    )
    .unwrap();
}

fn resolves(index: &IdentityIndex, platform: &str, id: &str, slug: &str) -> bool {
    match index.lookup(platform, id) {
        Some(page) => page.slug == slug,
        None => false,
    }
}

fn main() {
    let root = std::env::temp_dir().join(format!(
        "autoship-instagram-recovery-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let layout = WikiLayout::new(root);
    fs::create_dir_all(layout.people_dir()).unwrap();

    write_page(
        &layout,
        "recoverable",
        "close: true\naliases: Recover\nhousehold: mate\nidentities:\n  email: recover@example.test\n  phone: \\\"+15550000001\\\"\n  instagram:\n    handle: malformed",
    );
    write_page(
        &layout,
        "scalar-instagram",
        "identities:\n  instagram: scalar_handle",
    );
    write_page(
        &layout,
        "list-instagram",
        "identities:\n  instagram: [first_handle, second_handle]",
    );
    write_page(
        &layout,
        "unrecoverable",
        "identities:\n  email:\n    invalid: shape",
    );
    fs::write(
        layout.people_dir().join("broken.md"),
        "---\nidentities:\n  email: [broken,\n---\n# broken\n",
    )
    .unwrap();

    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = WarningSubscriber {
        events: Arc::clone(&events),
    };
    let index = tracing::subscriber::with_default(subscriber, || {
        IdentityIndex::build(&layout).unwrap()
    });

    let recovered_present = index.pages().iter().any(|page| page.slug == "recoverable");
    let valid_identities_resolve = resolves(&index, "email", "recover@example.test", "recoverable")
        && resolves(&index, "phone", "+15550000001", "recoverable");
    let malformed_instagram_misses = index.lookup("instagram", "malformed").is_none();
    let other_front_matter_preserved = index
        .pages()
        .iter()
        .find(|page| page.slug == "recoverable")
        .map(|page| {
            page.close
                && page.aliases == ["Recover"]
                && page.household == ["mate"]
        })
        .unwrap_or(false);
    let valid_instagram_shapes_remain = resolves(&index, "instagram", "scalar_handle", "scalar-instagram")
        && resolves(&index, "instagram", "first_handle", "list-instagram")
        && resolves(&index, "instagram", "second_handle", "list-instagram");

    let events = events.lock().unwrap();
    let recovery_warning = events.iter().any(|event| {
        event
            .get("message")
            .map(|message| message != "skipping page with bad front-matter")
            .unwrap_or(true)
            && event
                .values()
                .any(|value| value.contains("identities.instagram"))
    });
    let skip_for = |page_name: &str| {
        events.iter().any(|event| {
            event.get("message").map(String::as_str) == Some("skipping page with bad front-matter")
                && event.values().any(|value| value.contains(page_name))
        })
    };
    let recovered_not_skipped = !skip_for("recoverable.md");
    let unrecoverable_still_skipped = index.pages().iter().all(|page| page.slug != "unrecoverable")
        && index.pages().iter().all(|page| page.slug != "broken")
        && skip_for("unrecoverable.md")
        && skip_for("broken.md");

    println!(
        "[{recovered_present},{valid_identities_resolve},{malformed_instagram_misses},{other_front_matter_preserved},{valid_instagram_shapes_remain},{recovery_warning},{recovered_not_skipped},{unrecoverable_still_skipped},{}]",
        index.pages().iter().any(|page| page.slug == "recoverable")
    );

    let _ = fs::remove_dir_all(&layout.root);
}
