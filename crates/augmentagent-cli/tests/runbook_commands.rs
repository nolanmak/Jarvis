//! #1299 — every `augmentagent …` command in `docs/SLACK-RUNBOOK.md` must
//! exist in the binary, so the runbook cannot drift from the CLI.
//!
//! Commands are taken from fenced code blocks and inline code spans; for
//! each one the real binary parses the whole command line with `--help`
//! appended. clap reports an unknown subcommand or flag before it handles
//! `--help`, and `--help` stops before required arguments are checked or
//! anything runs, so a pass means "this command line is valid" and nothing
//! is executed. `<placeholders>` become a dummy value; shell pipes,
//! redirections and `VAR=value` prefixes are stripped.

use std::process::Command;

/// Read at test time, not `include_str!`: the updater rebuilds the daemon
/// whenever a compile-embedded file changes, and a runbook edit must not.
fn runbook() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/SLACK-RUNBOOK.md"
    ))
    .expect("docs/SLACK-RUNBOOK.md")
}

/// Replace every `<…>` placeholder with one dummy word.
fn fill_placeholders(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('<') {
        let Some(len) = rest[start..].find('>') else {
            break;
        };
        out.push_str(&rest[..start]);
        out.push_str("PLACEHOLDER");
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    out
}

/// Minimal shell-word split: whitespace, with '…' and "…" quoting.
fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The `augmentagent` argument vectors in one shell snippet.
fn commands_in(snippet: &str) -> Vec<Vec<String>> {
    let filled = fill_placeholders(snippet);
    let mut out = Vec::new();
    for segment in filled.split(['|', ';']).flat_map(|s| s.split("&&")) {
        let segment = segment.split('>').next().unwrap_or("");
        let mut w = words(segment);
        while w
            .first()
            .is_some_and(|t| t.contains('=') && !t.starts_with('-'))
        {
            w.remove(0);
        }
        if w.first().map(String::as_str) == Some("augmentagent") {
            out.push(w[1..].to_vec());
        }
    }
    out
}

/// Every command in a Markdown document: fenced blocks line by line,
/// inline code spans (which may wrap across lines) everywhere else.
fn commands(doc: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut prose = String::new();
    let mut in_fence = false;
    for line in doc.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            prose.push('\n');
            continue;
        }
        if in_fence {
            let l = line.trim();
            if !l.is_empty() && !l.starts_with('#') {
                out.extend(commands_in(l));
            }
        } else {
            prose.push_str(line);
            prose.push(' ');
        }
    }
    for (i, span) in prose.split('`').enumerate() {
        if i % 2 == 1 {
            out.extend(commands_in(span));
        }
    }
    out
}

/// `Ok` when the binary accepts `args` as a command line.
fn parses(args: &[String]) -> Result<(), String> {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .args(args)
        .arg("--help")
        .current_dir(tmp.path())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", tmp.path())
        .env("AUGMENTAGENT_DB", tmp.path().join("data.db"))
        .env(
            "AUGMENTAGENT_INSECURE_CREDENTIAL_DIR",
            tmp.path().join("creds"),
        )
        .env("AUGMENTAGENT_GH_DISABLE", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    if out.status.success() && stdout.contains("Usage:") {
        Ok(())
    } else {
        Err(format!(
            "`augmentagent {}` does not parse: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[test]
fn every_runbook_command_exists_in_the_cli() {
    let cmds = commands(&runbook());
    assert!(cmds.len() >= 30, "found only {} commands", cmds.len());
    let failures: Vec<String> = cmds.iter().filter_map(|c| parses(c).err()).collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn the_runbook_covers_every_lifecycle_step() {
    let cmds: Vec<String> = commands(&runbook()).iter().map(|c| c.join(" ")).collect();
    for needed in [
        "install autostart",
        "slack app manifest",
        "slack app install --stdin",
        "slack app status",
        "slack app verify",
        "slack app owner bind --user",
        "slack app owner control set --channel",
        "slack app owner control remove",
        "service --unit daemon restart",
        "status --json",
        "doctor --deep",
        "doctor --keychain-probe",
        "logs --unit daemon",
        "slack app rotate --stdin",
        "slack app owner unbind",
        "slack app remove",
        "uninstall autostart",
    ] {
        assert!(
            cmds.iter().any(|c| c.starts_with(needed)),
            "the runbook has no `augmentagent {needed}` step"
        );
    }
}

#[test]
fn the_checker_catches_a_command_that_does_not_exist() {
    let doc = "Run `augmentagent slack app instal --stdin` and\n\n```sh\naugmentagent doctor --no-such-flag\nAUGMENTAGENT_X=1 augmentagent status --json | jq .\n```\n";
    let cmds = commands(doc);
    assert_eq!(cmds.len(), 3, "{cmds:?}");
    assert!(
        cmds.contains(&vec!["status".to_string(), "--json".to_string()]),
        "env prefix and pipe stripped: {cmds:?}"
    );
    let failing: Vec<_> = cmds.iter().filter(|c| parses(c).is_err()).collect();
    assert_eq!(failing.len(), 2, "{failing:?}");
    // Placeholders, quotes and redirections.
    assert_eq!(
        commands_in("augmentagent slack app owner bind --user <your member ID> > out.json"),
        vec![vec![
            "slack",
            "app",
            "owner",
            "bind",
            "--user",
            "PLACEHOLDER"
        ]]
    );
}
