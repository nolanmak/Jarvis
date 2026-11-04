use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use augmentagent_wiki::{IdentityIndex, WikiLayout};

fn slug(index: &IdentityIndex, platform: &str, handle: &str) -> Option<String> {
    index.lookup(platform, handle).map(|page| page.slug.clone())
}

fn json_string(value: Option<String>) -> String {
    match value {
        Some(value) => format!("\"{value}\""),
        None => "null".to_string(),
    }
}

fn main() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("autoship-instagram-{unique}"));
    let layout = WikiLayout::new(&root);
    fs::create_dir_all(layout.people_dir()).unwrap();

    fs::write(
        layout.people_dir().join("scalar-person.md"),
        "---\nidentities:\n  instagram: scalar_handle\n---\n",
    )
    .unwrap();
    fs::write(
        layout.people_dir().join("sequence-person.md"),
        "---\nidentities:\n  instagram:\n    - first_handle\n    - second_handle\n---\n",
    )
    .unwrap();
    fs::write(
        layout.people_dir().join("regression-person.md"),
        "---\nidentities:\n  email: Person@Example.test\n  discord: \"123\"\n---\n",
    )
    .unwrap();

    let index = IdentityIndex::build(&layout).unwrap();
    let observed = [
        json_string(slug(&index, "instagram", "scalar_handle")),
        json_string(slug(&index, "instagram", "first_handle")),
        json_string(slug(&index, "instagram", "second_handle")),
        index.len().to_string(),
        json_string(slug(&index, "email", "person@example.test")),
        json_string(slug(&index, "discord", "999")),
    ];
    println!("[{}]", observed.join(","));
    let _ = fs::remove_dir_all(root);
}
