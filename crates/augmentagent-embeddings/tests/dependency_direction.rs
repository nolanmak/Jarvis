//! `augmentagent-embeddings` depends on `augmentagent-messages`, never the
//! reverse (#1132). A cycle would be caught by cargo, but a reversed
//! direction (messages growing an embeddings dependency) would not.

#[test]
fn messages_crate_does_not_depend_on_embeddings() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../augmentagent-messages/Cargo.toml");
    let text = std::fs::read_to_string(&manifest).unwrap();
    assert!(
        !text.contains("augmentagent-embeddings"),
        "augmentagent-messages must not depend on augmentagent-embeddings"
    );
    let ours = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
    )
    .unwrap();
    assert!(
        ours.contains("augmentagent-messages"),
        "embeddings reuses the messages crate's text preparation"
    );
}
