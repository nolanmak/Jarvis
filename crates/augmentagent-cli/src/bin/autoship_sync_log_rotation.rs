#[path = "../log_rotate.rs"]
mod log_rotate;

use chrono::NaiveDate;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

fn archive_count(dir: &Path, name: &str) -> usize {
    log_rotate::rotated_files(dir, name).len()
}

fn main() {
    std::env::set_var("AUGMENTAGENT_SYNC_LOG_ROTATE_MB", "10");
    std::env::set_var("AUGMENTAGENT_SYNC_LOG_KEEP", "5");
    let defaults = log_rotate::Policy::from_env();
    let default_values = [defaults.max_bytes / (1024 * 1024), defaults.keep_months as u64];

    std::env::set_var("AUGMENTAGENT_SYNC_LOG_ROTATE_MB", "2");
    std::env::set_var("AUGMENTAGENT_SYNC_LOG_KEEP", "4");
    let overridden = log_rotate::Policy::from_env();
    let override_values = [overridden.max_bytes / (1024 * 1024), overridden.keep_months as u64];

    std::env::set_var("AUGMENTAGENT_SYNC_LOG_ROTATE_MB", "10");
    std::env::set_var("AUGMENTAGENT_SYNC_LOG_KEEP", "5");
    let policy = log_rotate::Policy::from_env();
    let root = std::env::temp_dir().join(format!("autoship-sync-log-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let names = ["imessage-sync.log", "whatsapp-sync.log", "apple-notes-sync.log"];
    let payload = vec![b'x'; 10 * 1024 * 1024 + 1];
    let today = NaiveDate::from_ymd_opt(2026, 11, 20).unwrap();

    let live = root.join(names[0]);
    let mut held = OpenOptions::new().create(true).append(true).open(&live).unwrap();
    held.write_all(&payload).unwrap();
    held.flush().unwrap();
    for name in &names[1..] {
        fs::write(root.join(name), &payload).unwrap();
    }
    log_rotate::run(&root, &policy, today, false, false);
    let initially_rotated = names.iter().all(|name| {
        let live_path = root.join(name);
        live_path.exists()
            && fs::metadata(&live_path).map(|m| m.len() < 10 * 1024 * 1024).unwrap_or(false)
            && archive_count(&root, name) >= 1
    });

    fs::write(root.join("imessage-sync.log.fda-spam-20260928.gz"), b"manual").unwrap();
    for _ in 0..5 {
        held.write_all(&payload).unwrap();
        held.flush().unwrap();
        log_rotate::run(&root, &policy, today, false, false);
    }
    held.write_all(b"after-rotation\n").unwrap();
    held.flush().unwrap();
    let append_visible = fs::read_to_string(&live).map(|s| s.ends_with("after-rotation\n")).unwrap_or(false);
    let archive_total = archive_count(&root, names[0]);
    let manual_preserved = root.join("imessage-sync.log.fda-spam-20260928.gz").exists();
    let compressed = log_rotate::rotated_files(&root, names[0]).iter().all(|(p, _)| p.extension().and_then(|x| x.to_str()) == Some("gz"));

    println!("[[{},{}],[{},{}],{},{},{},{},{}]", default_values[0], default_values[1], override_values[0], override_values[1], initially_rotated && compressed, fs::metadata(&live).map(|m| m.len() < 10 * 1024 * 1024).unwrap_or(false), append_visible, archive_total, manual_preserved);
    let _ = fs::remove_dir_all(root);
}
