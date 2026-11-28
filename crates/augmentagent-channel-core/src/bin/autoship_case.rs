//! Autoship observation for the capability manifest, embedded from a
//! `src/bin/` target.
//!
//! `include_str!` resolves against the embedding file, so this path carries
//! one directory level more than the copy in `tests/capability_inventory.rs`:
//! `src/bin/` sits below the crate root, `tests/` sits at it. Naming the
//! manifest from here is what makes building this target a check on that
//! depth; `tests/capability_inventory.rs` runs the built binary, so a focused
//! run of that test compiles this embed.
const MANIFEST: &str = include_str!("../../../../docs/reasoner-capabilities.json");

fn main() {
    let manifest: serde_json::Value = serde_json::from_str(MANIFEST).unwrap();
    let callsites: Vec<&str> = manifest["presets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|preset| preset["callsite"].as_str().unwrap())
        .collect();
    println!("{}", serde_json::to_string(&callsites).unwrap());
}
