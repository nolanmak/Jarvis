//! `augmentagent setup harvest <channel>` — cookie-harvest schema emitter.
//!
//! Two modes, both serving the same goal (get credentials into the keychain):
//!
//! 1. Default (interactive). Equivalent to running
//!    `./scripts/<channel>-harvest.sh` directly — we just `exec` the script
//!    with inherited stdio so its `read` prompts talk straight to the tty.
//!    The script writes a temp JSON file and ultimately calls
//!    `augmentagent <channel> login --creds-json <path>` itself.
//!
//! 2. `--non-interactive --json`. Print the field schema for the channel
//!    (label, hint, secret bool, plus the `next_cmd` to run once values are
//!    collected) and exit 0 without touching the script. The `/setup` skill
//!    consumes this output, drives the operator through `AskUserQuestion`,
//!    writes a temp JSON file at `--creds-out`, then invokes the channel's
//!    `login --creds-json` itself.
//!
//! Schema entries are HARDCODED here — they mirror what the corresponding
//! `scripts/<channel>-harvest.sh` actually prompts for. If a script grows a
//! new field, update the matching entry in `SCHEMA` below. Do NOT try to
//! parse the scripts at runtime; they may not be present in every install
//! (e.g. on the daemon host after a stripped deploy).

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result};
use clap::{Args, ValueEnum};
use serde_json::{json, Value};
use tokio::process::Command;

/// `augmentagent setup harvest <channel> [flags]` parsed args.
#[derive(Args, Debug, Clone)]
pub struct HarvestArgs {
    /// Cookie-harvest channel to drive. Each value maps to a
    /// `scripts/<channel>-harvest.sh` (LinkedIn exposes a second
    /// browser-intercept method too).
    #[arg(value_enum)]
    pub channel: HarvestChannel,

    /// Skip the interactive shell script and just emit the field schema
    /// (combine with `--json`). The `/setup` skill uses this to collect
    /// values through `AskUserQuestion` and write the temp creds file
    /// itself before calling `augmentagent <channel> login --creds-json`.
    #[arg(long, default_value_t = false)]
    pub non_interactive: bool,

    /// Emit machine-readable JSON. Required together with
    /// `--non-interactive`; ignored otherwise (the script renders its own
    /// prompts).
    #[arg(long, default_value_t = false)]
    pub json: bool,

    /// Where the `/setup` skill plans to write the temp creds JSON. Only
    /// meaningful in `--non-interactive --json` mode — it's copied verbatim
    /// into the output as `expected_creds_path` so the skill can echo it
    /// back into the `login --creds-json <path>` call.
    #[arg(long, value_name = "PATH")]
    pub creds_out: Option<PathBuf>,
}

/// One value per channel that has a `scripts/<channel>-harvest.sh`. Kept in
/// sync with `SCHEMA` below — adding a channel here without a schema entry
/// will trip the `expect()` in `schema_for`.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[value(rename_all = "lowercase")]
pub enum HarvestChannel {
    Discord,
    Twitter,
    Linkedin,
    Instagram,
}

impl HarvestChannel {
    fn as_str(self) -> &'static str {
        match self {
            HarvestChannel::Discord => "discord",
            HarvestChannel::Twitter => "twitter",
            HarvestChannel::Linkedin => "linkedin",
            HarvestChannel::Instagram => "instagram",
        }
    }
}

/// One cookie/header value the operator pastes from devtools.
#[derive(Debug, Clone)]
pub struct Field {
    /// JSON key the channel's `login --creds-json` expects.
    pub name: &'static str,
    /// Human label shown by the `/setup` skill in `AskUserQuestion`.
    pub label: &'static str,
    /// Devtools hint — where in Chrome/Firefox the value lives.
    pub hint: &'static str,
    /// Treat as a password (mask in transcripts; don't echo). True for
    /// session cookies and auth tokens.
    pub secret: bool,
    /// Whether the operator may skip this prompt (e.g. Instagram `rur`,
    /// `username`).
    pub optional: bool,
}

/// One way to harvest a given channel. Most channels have exactly one
/// method; LinkedIn exposes two (devtools cookies + browser intercept).
#[derive(Debug, Clone)]
pub struct HarvestMethod {
    /// Stable id (e.g. `devtools_cookies`, `browser_intercept`). The
    /// `/setup` skill uses this to branch on which prompt set to render.
    pub name: &'static str,
    /// Short human label.
    pub label: &'static str,
    /// Repo-relative path to the shell script that implements this method.
    pub script_path: &'static str,
    /// Free-form walkthrough lines the `/setup` skill can echo verbatim.
    pub doc_steps: &'static [&'static str],
    /// Fields the operator must paste. Empty for methods that auto-extract
    /// (LinkedIn intercept).
    pub fields: &'static [Field],
}

/// Top-level schema entry per channel.
#[derive(Debug, Clone)]
pub struct ChannelSchema {
    /// Channel slug (e.g. `discord`).
    pub channel: &'static str,
    /// Page the operator should open to start harvesting.
    pub instructions_url: &'static str,
    /// All available methods (>= 1).
    pub methods: &'static [HarvestMethod],
    /// The `augmentagent` invocation that consumes the creds JSON. The
    /// `<path>` placeholder is replaced by the temp file the `/setup`
    /// skill writes.
    pub next_cmd: &'static str,
}

// ---------------------------------------------------------------------------
// SCHEMA — hardcoded, in lockstep with scripts/<channel>-harvest.sh.
// If you add a field to a script, mirror it here and bump the test below.
// ---------------------------------------------------------------------------

const DISCORD_FIELDS: &[Field] = &[
    Field {
        name: "user_id",
        label: "Discord user_id (numeric)",
        hint: "Found in /api/v9/users/@me response or in any message's author.id; typical 18-digit number.",
        secret: false,
        optional: false,
    },
    Field {
        name: "token",
        label: "authorization header value",
        hint: "Request Headers -> 'authorization' — the raw token, NO 'Bearer' prefix. Starts with MTE/MTA/MTI/...",
        secret: true,
        optional: false,
    },
    Field {
        name: "super_properties_b64",
        label: "x-super-properties header",
        hint: "Request Headers -> 'x-super-properties' — the full base64-encoded fingerprint, starts with 'eyJ'.",
        secret: true,
        optional: false,
    },
    Field {
        name: "user_agent",
        label: "user-agent header",
        hint: "Request Headers -> 'user-agent'. Must match the browser_user_agent field inside the decoded x-super-properties.",
        secret: false,
        optional: false,
    },
];

const TWITTER_FIELDS: &[Field] = &[
    Field {
        name: "user_id",
        label: "X numeric user_id",
        hint: "Devtools -> Network -> reload -> any request to /i/api/graphql/* or /i/api/1.1/* — your numeric id is on the response. ~19 digits.",
        secret: false,
        optional: false,
    },
    Field {
        name: "screen_name",
        label: "@handle without the @",
        hint: "Your X handle minus the leading @ (e.g. 'nolanmak').",
        secret: false,
        optional: false,
    },
    Field {
        name: "auth_token",
        label: "auth_token cookie",
        hint: "Devtools -> Application -> Cookies -> https://x.com -> 'auth_token' value column.",
        secret: true,
        optional: false,
    },
    Field {
        name: "ct0",
        label: "ct0 cookie",
        hint: "Devtools -> Application -> Cookies -> https://x.com -> 'ct0' value column.",
        secret: true,
        optional: false,
    },
];

const LINKEDIN_DEVTOOLS_FIELDS: &[Field] = &[
    Field {
        name: "member_urn",
        label: "your fsd_profile URN",
        hint: "On linkedin.com/messaging -> devtools Network -> reload -> any voyager/api/* request -> body/URL has 'urn:li:fsd_profile:ACoAA...'. Copy the whole URN.",
        secret: false,
        optional: false,
    },
    Field {
        name: "li_at",
        label: "li_at cookie",
        hint: "Devtools -> Application -> Cookies -> https://www.linkedin.com -> 'li_at' value column.",
        secret: true,
        optional: false,
    },
    Field {
        name: "JSESSIONID",
        label: "JSESSIONID cookie",
        hint: "Devtools -> Application -> Cookies -> https://www.linkedin.com -> 'JSESSIONID' value column. Paste WITH the surrounding quotes (e.g. \"ajax:0103...\").",
        secret: true,
        optional: false,
    },
    Field {
        name: "bcookie",
        label: "bcookie cookie",
        hint: "Devtools -> Application -> Cookies -> https://www.linkedin.com -> 'bcookie' value column.",
        secret: true,
        optional: false,
    },
];

const INSTAGRAM_FIELDS: &[Field] = &[
    Field {
        name: "ds_user_id",
        label: "ds_user_id (numeric account id)",
        hint: "Devtools -> Application -> Cookies -> https://www.instagram.com -> 'ds_user_id' value column.",
        secret: false,
        optional: false,
    },
    Field {
        name: "username",
        label: "your @handle (informational)",
        hint: "Your Instagram handle (no @). Used only for display; safe to skip.",
        secret: false,
        optional: true,
    },
    Field {
        name: "sessionid",
        label: "sessionid cookie",
        hint: "Devtools -> Application -> Cookies -> https://www.instagram.com -> 'sessionid' value column.",
        secret: true,
        optional: false,
    },
    Field {
        name: "csrftoken",
        label: "csrftoken cookie",
        hint: "Devtools -> Application -> Cookies -> https://www.instagram.com -> 'csrftoken' value column.",
        secret: true,
        optional: false,
    },
    Field {
        name: "mid",
        label: "mid cookie",
        hint: "Devtools -> Application -> Cookies -> https://www.instagram.com -> 'mid' value column.",
        secret: true,
        optional: false,
    },
    Field {
        name: "ig_did",
        label: "ig_did cookie",
        hint: "Devtools -> Application -> Cookies -> https://www.instagram.com -> 'ig_did' value column.",
        secret: true,
        optional: false,
    },
    Field {
        name: "rur",
        label: "rur cookie (optional)",
        hint: "Devtools -> Application -> Cookies -> https://www.instagram.com -> 'rur' if present. Safe to skip.",
        secret: true,
        optional: true,
    },
];

const DISCORD_METHODS: &[HarvestMethod] = &[HarvestMethod {
    name: "devtools_headers",
    label: "Copy headers from Chrome devtools",
    script_path: "scripts/discord-harvest.sh",
    doc_steps: &[
        "Open https://discord.com/app in Chrome and log in.",
        "Open DevTools -> Network tab -> filter 'messages'.",
        "Click any channel so a request fires.",
        "Pick any request to discord.com/api/v9/... and copy the four header fields below.",
    ],
    fields: DISCORD_FIELDS,
}];

const TWITTER_METHODS: &[HarvestMethod] = &[HarvestMethod {
    name: "devtools_cookies",
    label: "Copy cookies + ids from Chrome devtools",
    script_path: "scripts/twitter-harvest.sh",
    doc_steps: &[
        "Open https://x.com/messages in Chrome and log in.",
        "DevTools -> Application -> Storage -> Cookies -> https://x.com.",
        "Copy auth_token and ct0 from the Value column.",
        "DevTools -> Network -> reload -> any /i/api/graphql/* request -> numeric user_id is on the response.",
        "screen_name is your @handle minus the @.",
    ],
    fields: TWITTER_FIELDS,
}];

const LINKEDIN_METHODS: &[HarvestMethod] = &[
    HarvestMethod {
        name: "devtools_cookies",
        label: "Copy cookies from Chrome devtools",
        script_path: "scripts/linkedin-harvest.sh",
        doc_steps: &[
            "Open https://www.linkedin.com/messaging/ in Chrome and log in.",
            "DevTools -> Application -> Storage -> Cookies -> https://www.linkedin.com.",
            "Copy li_at, JSESSIONID, bcookie values.",
            "On the messaging page, DevTools -> Network -> reload -> any voyager/api/* request -> body/URL contains 'urn:li:fsd_profile:ACoAA...'. Copy the whole URN.",
        ],
        fields: LINKEDIN_DEVTOOLS_FIELDS,
    },
    HarvestMethod {
        name: "browser_intercept",
        label: "Auto-extract from Claude Intercept capture DB",
        script_path: "scripts/linkedin-harvest-from-intercept.sh",
        doc_steps: &[
            "Requires a prior /intercept run that captured logged-in linkedin.com traffic.",
            "Runs without prompts; reads cookies + URN straight from the captures.db.",
            "Override the DB path with CAPTURES_DB=<path> if non-default.",
        ],
        fields: &[],
    },
];

const INSTAGRAM_METHODS: &[HarvestMethod] = &[HarvestMethod {
    name: "devtools_cookies",
    label: "Copy cookies from Chrome devtools",
    script_path: "scripts/instagram-harvest.sh",
    doc_steps: &[
        "Open https://www.instagram.com/ in Chrome and log in.",
        "DevTools -> Application -> Storage -> Cookies -> https://www.instagram.com.",
        "Copy ds_user_id, sessionid, csrftoken, mid, ig_did from the Value column.",
        "rur is optional; copy it if it's present in the cookies list.",
    ],
    fields: INSTAGRAM_FIELDS,
}];

const SCHEMA: &[ChannelSchema] = &[
    ChannelSchema {
        channel: "discord",
        instructions_url: "https://discord.com/app",
        methods: DISCORD_METHODS,
        next_cmd: "augmentagent discord login --creds-json <path>",
    },
    ChannelSchema {
        channel: "twitter",
        instructions_url: "https://x.com/messages",
        methods: TWITTER_METHODS,
        next_cmd: "augmentagent twitter login --session-json <path>",
    },
    ChannelSchema {
        channel: "linkedin",
        instructions_url: "https://www.linkedin.com/messaging/",
        methods: LINKEDIN_METHODS,
        next_cmd: "augmentagent linkedin login --cookies-json <path>",
    },
    ChannelSchema {
        channel: "instagram",
        instructions_url: "https://www.instagram.com/",
        methods: INSTAGRAM_METHODS,
        next_cmd: "augmentagent instagram login --cookies-json <path>",
    },
];

fn schema_for(ch: HarvestChannel) -> &'static ChannelSchema {
    SCHEMA
        .iter()
        .find(|s| s.channel == ch.as_str())
        .expect("SCHEMA missing entry for HarvestChannel variant — keep them in sync")
}

/// Hand-roll the JSON document (`serde_json::json!` only — no `serde`
/// derive dep on this crate). Mirrors the layout of `ChannelSchema` plus
/// the dynamic `expected_creds_path` field driven by `--creds-out`.
fn emit_json(schema: &ChannelSchema, creds_out: Option<&PathBuf>) -> Value {
    let methods: Vec<Value> = schema
        .methods
        .iter()
        .map(|m| {
            let fields: Vec<Value> = m
                .fields
                .iter()
                .map(|f| {
                    json!({
                        "name": f.name,
                        "label": f.label,
                        "hint": f.hint,
                        "secret": f.secret,
                        "optional": f.optional,
                    })
                })
                .collect();
            json!({
                "name": m.name,
                "label": m.label,
                "script_path": m.script_path,
                "doc_steps": m.doc_steps,
                "fields": fields,
            })
        })
        .collect();

    let mut doc = serde_json::Map::new();
    doc.insert("channel".into(), json!(schema.channel));
    doc.insert("instructions_url".into(), json!(schema.instructions_url));
    doc.insert("methods".into(), Value::Array(methods));
    doc.insert("next_cmd".into(), json!(schema.next_cmd));
    if let Some(p) = creds_out {
        doc.insert("expected_creds_path".into(), json!(p.display().to_string()));
    }
    Value::Object(doc)
}

/// Default interactive entrypoint: `exec` the channel's harvest script with
/// inherited stdio so its `read` prompts speak to the tty directly.
async fn run_interactive(channel: HarvestChannel) -> Result<()> {
    let schema = schema_for(channel);
    // Interactive mode always uses the first listed method (devtools-paste).
    // LinkedIn's `browser_intercept` is only surfaced via the JSON schema —
    // the skill picks it when it knows /intercept has captures.
    let method = schema
        .methods
        .first()
        .expect("every channel must have >= 1 method");
    let script = method.script_path;

    let mut cmd = Command::new("bash");
    cmd.arg(script)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = cmd
        .status()
        .await
        .with_context(|| format!("spawning {script} (is bash on $PATH and the script readable?)"))?;
    if let Some(code) = status.code() {
        if code != 0 {
            std::process::exit(code);
        }
        return Ok(());
    }
    // Killed by signal — propagate as nonzero.
    anyhow::bail!("{} terminated by signal", script);
}

/// `--non-interactive --json` entrypoint: dump the schema for the channel.
fn run_emit_json(channel: HarvestChannel, creds_out: Option<&PathBuf>) -> Result<()> {
    let schema = schema_for(channel);
    let doc = emit_json(schema, creds_out);
    let s = serde_json::to_string_pretty(&doc)
        .context("serializing harvest schema to JSON")?;
    println!("{s}");
    Ok(())
}

/// Public entrypoint for `augmentagent setup harvest …`.
pub async fn run_harvest(args: &HarvestArgs) -> Result<()> {
    if args.non_interactive {
        if !args.json {
            anyhow::bail!(
                "--non-interactive requires --json (this mode only emits the field schema; \
                 if you want the script to run, drop --non-interactive)"
            );
        }
        return run_emit_json(args.channel, args.creds_out.as_ref());
    }
    // Default: interactive. `--creds-out` is meaningless here — the script
    // picks its own temp path — but accept it silently so the `/setup` skill
    // can pass the same flag in both modes.
    run_interactive(args.channel).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_table_covers_every_variant() {
        for ch in [
            HarvestChannel::Discord,
            HarvestChannel::Twitter,
            HarvestChannel::Linkedin,
            HarvestChannel::Instagram,
        ] {
            // Panics if any variant is missing from SCHEMA.
            let _ = schema_for(ch);
        }
    }

    #[test]
    fn every_method_points_at_a_scripts_path() {
        for entry in SCHEMA {
            for m in entry.methods {
                assert!(
                    m.script_path.starts_with("scripts/"),
                    "method {} for channel {} should reference a scripts/ path",
                    m.name,
                    entry.channel
                );
            }
        }
    }

    #[test]
    fn linkedin_exposes_both_methods() {
        let li = schema_for(HarvestChannel::Linkedin);
        let names: Vec<&str> = li.methods.iter().map(|m| m.name).collect();
        assert!(names.contains(&"devtools_cookies"));
        assert!(names.contains(&"browser_intercept"));
    }

    #[test]
    fn discord_field_names_match_script() {
        // Discord script prompts for: user_id, token, super_properties_b64, user_agent
        let d = schema_for(HarvestChannel::Discord);
        let names: Vec<&str> = d.methods[0].fields.iter().map(|f| f.name).collect();
        assert_eq!(
            names,
            vec!["user_id", "token", "super_properties_b64", "user_agent"]
        );
    }

    #[test]
    fn twitter_field_names_match_script() {
        let t = schema_for(HarvestChannel::Twitter);
        let names: Vec<&str> = t.methods[0].fields.iter().map(|f| f.name).collect();
        assert_eq!(names, vec!["user_id", "screen_name", "auth_token", "ct0"]);
    }

    #[test]
    fn linkedin_devtools_field_names_match_script() {
        let li = schema_for(HarvestChannel::Linkedin);
        let dev = li
            .methods
            .iter()
            .find(|m| m.name == "devtools_cookies")
            .expect("linkedin must expose devtools_cookies");
        let names: Vec<&str> = dev.fields.iter().map(|f| f.name).collect();
        assert_eq!(
            names,
            vec!["member_urn", "li_at", "JSESSIONID", "bcookie"]
        );
    }

    #[test]
    fn linkedin_intercept_has_no_fields() {
        let li = schema_for(HarvestChannel::Linkedin);
        let icp = li
            .methods
            .iter()
            .find(|m| m.name == "browser_intercept")
            .expect("linkedin must expose browser_intercept");
        assert!(icp.fields.is_empty());
    }

    #[test]
    fn instagram_field_names_match_script() {
        let ig = schema_for(HarvestChannel::Instagram);
        let names: Vec<&str> = ig.methods[0].fields.iter().map(|f| f.name).collect();
        assert_eq!(
            names,
            vec![
                "ds_user_id",
                "username",
                "sessionid",
                "csrftoken",
                "mid",
                "ig_did",
                "rur"
            ]
        );
    }

    #[test]
    fn secrets_are_flagged_for_session_tokens() {
        // Spot-check: auth tokens / session cookies must be flagged secret
        // so the /setup skill masks them in transcripts.
        let d = schema_for(HarvestChannel::Discord);
        let token = d.methods[0]
            .fields
            .iter()
            .find(|f| f.name == "token")
            .unwrap();
        assert!(token.secret);

        let t = schema_for(HarvestChannel::Twitter);
        let auth = t.methods[0]
            .fields
            .iter()
            .find(|f| f.name == "auth_token")
            .unwrap();
        assert!(auth.secret);

        let li = schema_for(HarvestChannel::Linkedin);
        let li_at = li.methods[0]
            .fields
            .iter()
            .find(|f| f.name == "li_at")
            .unwrap();
        assert!(li_at.secret);

        let ig = schema_for(HarvestChannel::Instagram);
        let sid = ig.methods[0]
            .fields
            .iter()
            .find(|f| f.name == "sessionid")
            .unwrap();
        assert!(sid.secret);
    }

    #[test]
    fn emitted_schema_json_includes_expected_creds_path() {
        let schema = schema_for(HarvestChannel::Discord);
        let path = PathBuf::from("/tmp/discord-creds.json");
        let doc = emit_json(schema, Some(&path));
        let s = serde_json::to_string(&doc).unwrap();
        assert!(s.contains("\"expected_creds_path\":\"/tmp/discord-creds.json\""));
        assert!(s.contains("\"channel\":\"discord\""));
    }

    #[test]
    fn emitted_schema_omits_creds_path_when_absent() {
        let schema = schema_for(HarvestChannel::Discord);
        let doc = emit_json(schema, None);
        let s = serde_json::to_string(&doc).unwrap();
        assert!(!s.contains("expected_creds_path"));
    }

    #[test]
    fn emitted_schema_linkedin_lists_both_methods() {
        let schema = schema_for(HarvestChannel::Linkedin);
        let doc = emit_json(schema, None);
        let methods = doc.get("methods").and_then(|m| m.as_array()).unwrap();
        assert_eq!(methods.len(), 2);
        let names: Vec<&str> = methods
            .iter()
            .map(|m| m.get("name").and_then(|n| n.as_str()).unwrap())
            .collect();
        assert!(names.contains(&"devtools_cookies"));
        assert!(names.contains(&"browser_intercept"));
    }
}
