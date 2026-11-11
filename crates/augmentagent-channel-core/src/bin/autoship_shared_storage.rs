use std::path::Path;

fn contains_all(text: &str, needles: &[&str]) -> bool {
    needles.iter().all(|needle| text.contains(needle))
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let core_lib = std::fs::read_to_string(root.join("crates/augmentagent-channel-core/src/lib.rs"))
        .expect("read channel-core library");
    let cli_main = std::fs::read_to_string(root.join("crates/augmentagent-cli/src/main.rs"))
        .expect("read CLI entry point");
    let docs = std::fs::read_to_string(root.join("docs/disk-storage.md"))
        .expect("read storage documentation");

    let core_authority = contains_all(
        &core_lib,
        &[
            "pub mod build_storage",
            "pub mod storage_leases",
            "pub mod storage_reservations",
            "pub mod build_storage_ci_holds",
        ],
    );
    let cli_reexports_core = cli_main.contains("pub use augmentagent_channel_core::")
        && cli_main.contains("storage_reservations");
    let cli_no_longer_owns_authority = !contains_all(
        &cli_main,
        &[
            "mod build_storage;",
            "mod storage_leases;",
            "mod storage_reservations;",
        ],
    );
    let documents_shared_implementation = docs.contains("channel-core")
        && docs.contains("shared storage");

    println!(
        "{}",
        serde_json::to_string(&[
            core_authority,
            cli_reexports_core,
            cli_no_longer_owns_authority,
            documents_shared_implementation,
        ])
        .expect("serialize observations")
    );
}
