//! Snapshot test: pin the rendered `code_mode_system(&manifest_v1())` output
//! to a golden file so silent prompt drift is caught on CI.
//!
//! When the prompt is intentionally changed, regenerate the fixture by
//! running:
//!
//!     UPDATE_SNAPSHOTS=1 cargo test -p augmentagent-channel-core \
//!         --test code_mode_prompt_snapshot
//!
//! and commit the updated `tests/snapshots/code_mode_system_v1.txt`.

use std::fs;
use std::path::PathBuf;

use augmentagent_channel_core::code_mode::manifest_v1;
use augmentagent_channel_core::prompt::code_mode_system;

fn snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("snapshots")
        .join("code_mode_system_v1.txt")
}

#[test]
fn code_mode_system_v1_matches_pinned_snapshot() {
    let rendered = code_mode_system(&manifest_v1());
    let path = snapshot_path();

    if std::env::var("UPDATE_SNAPSHOTS").is_ok() {
        fs::create_dir_all(path.parent().unwrap()).expect("mkdir snapshots");
        fs::write(&path, &rendered).expect("write snapshot");
        return;
    }

    let expected = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing snapshot at {} ({e}). Run with UPDATE_SNAPSHOTS=1 to write it.",
            path.display()
        )
    });

    if expected != rendered {
        // Emit a focused diff hint: where the first divergence is.
        let mut first_diff = expected.len().min(rendered.len());
        for (i, (a, b)) in expected.bytes().zip(rendered.bytes()).enumerate() {
            if a != b {
                first_diff = i;
                break;
            }
        }
        let ctx = first_diff.saturating_sub(40);
        panic!(
            "code_mode_system(manifest_v1()) drifted from snapshot.\n\
             First byte differs at offset {first_diff} (file: {path}).\n\
             Snapshot:  {snap_excerpt:?}\n\
             Rendered:  {got_excerpt:?}\n\
             Run with UPDATE_SNAPSHOTS=1 to regenerate.",
            path = path.display(),
            snap_excerpt = &expected[ctx..(first_diff + 40).min(expected.len())],
            got_excerpt = &rendered[ctx..(first_diff + 40).min(rendered.len())],
        );
    }
}
