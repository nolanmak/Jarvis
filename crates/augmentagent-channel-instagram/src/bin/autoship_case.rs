use std::path::PathBuf;

use augmentagent_channel_instagram::feed::close_instagram_contacts;
use augmentagent_wiki::WikiLayout;

fn observe(case: &str, instagram: &str) -> Option<String> {
    let root: PathBuf = std::env::temp_dir().join(format!(
        "autoship-instagram-list-{}-{}",
        std::process::id(),
        case
    ));
    let layout = WikiLayout::new(&root);
    std::fs::create_dir_all(layout.people_dir()).unwrap();
    std::fs::write(
        layout.people_dir().join("contact.md"),
        format!(
            "---\nkind: person\nclose: true\nidentities:\n  instagram: {instagram}\n---\n"
        ),
    )
    .unwrap();
    let result = close_instagram_contacts(&layout)
        .into_iter()
        .next()
        .map(|contact| contact.instagram_handle);
    std::fs::remove_dir_all(root).unwrap();
    result
}

fn main() {
    let observed = vec![
        observe("at-only", "[\"@\", \"real_ig\"]"),
        observe("blank", "[\"\", \"real_ig\"]"),
        observe("first", "[\"@first_ig\", \"second_ig\"]"),
        observe("none", "[\"@\", \"\"]"),
    ];
    println!("{}", serde_json::to_string(&observed).unwrap());
}
