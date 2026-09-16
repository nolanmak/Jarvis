//! #1046 test support: drive the REAL `ClaudeCliReasoner` spawn path against
//! an argv-recording stub, so a test can prove which flags a production call
//! path hands the `claude` CLI. The stub runs no model and reads no account
//! data; it answers every call with one canned `result` event.

use std::path::PathBuf;

use augmentagent_channel_core::ClaudeCliReasoner;

/// `ClaudeCliReasoner` reads `CLAUDE_CLI` once, at construction. Serialize the
/// set-construct-restore window so parallel tests cannot swap stubs.
static CLAUDE_CLI_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) struct ArgvStub {
    dir: tempfile::TempDir,
    pub bin: PathBuf,
}

impl ArgvStub {
    /// A stub that answers every spawn with `result` as the final text.
    pub fn new(result: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let answer = dir.path().join("answer.jsonl");
        let event = serde_json::json!({ "type": "result", "result": result });
        std::fs::write(&answer, format!("{event}\n")).unwrap();
        let calls = dir.path().join("calls");
        std::fs::create_dir(&calls).unwrap();
        let bin = dir.path().join("fake-claude-argv");
        // One NUL-separated argv file per spawn, named by spawn time so the
        // listing sorts in call order.
        std::fs::write(
            &bin,
            format!(
                "#!/usr/bin/env bash\nset -euo pipefail\ncat >/dev/null\n\
                 out=$(mktemp \"{calls}/$(date +%s%N).XXXXXX\")\n\
                 printf '%s\\0' \"$@\" > \"$out\"\n\
                 cat \"{answer}\"\n",
                calls = calls.display(),
                answer = answer.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir, bin }
    }

    /// A production reasoner whose `CLAUDE_CLI` is this stub.
    pub fn reasoner(&self) -> ClaudeCliReasoner {
        let _guard = CLAUDE_CLI_ENV
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("CLAUDE_CLI");
        std::env::set_var("CLAUDE_CLI", &self.bin);
        let reasoner = ClaudeCliReasoner::new();
        match previous {
            Some(value) => std::env::set_var("CLAUDE_CLI", value),
            None => std::env::remove_var("CLAUDE_CLI"),
        }
        reasoner
    }

    /// The recorded argv of every spawn, in call order.
    pub fn calls(&self) -> Vec<Vec<String>> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(self.dir.path().join("calls"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        files.sort();
        files
            .iter()
            .map(|file| {
                // Every arg ends in NUL; an empty arg (`--allowedTools ''`)
                // is a bare NUL and must survive the split.
                let raw = std::fs::read(file).unwrap();
                let raw = raw.strip_suffix(&[0]).unwrap_or(&raw);
                raw.split(|byte| *byte == 0)
                    .map(|arg| String::from_utf8_lossy(arg).into_owned())
                    .collect()
            })
            .collect()
    }
}

/// `argv` on one line with long values (the system prompt) elided, so a
/// receipt shows the real flag shape without repeating prompt text.
pub(crate) fn summarize(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| match arg.len() {
            0 => "''".to_string(),
            1..=48 => arg.clone(),
            bytes => format!("<{bytes} bytes>"),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The value following `name` in `argv`, if the flag is present.
pub(crate) fn flag<'a>(argv: &'a [String], name: &str) -> Option<&'a str> {
    argv.iter()
        .position(|arg| arg == name)
        .and_then(|index| argv.get(index + 1))
        .map(String::as_str)
}
