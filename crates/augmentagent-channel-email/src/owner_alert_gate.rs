//! Additive, offline-replayable owner gate. No model calls or network sends.
use super::owner_alerts::AlertPlan;
use augmentagent_store::{
    owner_alert_gate::{bare_sender, selector_matches},
    owner_alerts::Urgency,
    Email, Store,
};
use augmentagent_wiki::{identity::IdentityIndex, WikiLayout};
use std::{path::Path, sync::OnceLock};

pub const ENABLED: &str = "AUGMENTAGENT_ALERT_EMAIL_GATES";
pub fn enabled() -> bool {
    std::env::var(ENABLED).is_ok_and(|v| matches!(v.as_str(), "1" | "true"))
}
#[derive(Default)]
pub struct Catalog {
    people: Vec<Person>,
}
struct Person {
    slug: String,
    emails: Vec<String>,
    relationship: Option<String>,
}
impl Catalog {
    pub fn load(root: Option<&Path>) -> anyhow::Result<Self> {
        let Some(root) = root else {
            return Ok(Self::default());
        };
        let canonical = root.canonicalize()?;
        let index = IdentityIndex::build(&WikiLayout::new(root.to_path_buf()))?;
        let mut people = Vec::new();
        for person in index.pages() {
            let path = person.path.canonicalize()?;
            if !path.starts_with(&canonical) {
                continue;
            }
            let text = std::fs::read_to_string(path)?;
            let relationship = text.lines().find_map(|line| {
                let trimmed = line.trim().trim_start_matches(['-', '*']).trim();
                static DECLARATION: OnceLock<regex::Regex> = OnceLock::new();
                let declaration = DECLARATION.get_or_init(|| {
                    regex::Regex::new(
                    r"(?i)(?:^|[.!?]\s+)(?:an?\s+)?(?:active[- ]collaborator|close[- ]contact)\b",
                )
                .expect("constant relationship declaration")
                });
                if declaration.is_match(trimmed) {
                    return Some(line.trim().to_string());
                }
                let (key, value) = trimmed.split_once(':')?;
                if !key.eq_ignore_ascii_case("relationship") {
                    return None;
                }
                let value = value
                    .trim()
                    .trim_matches(['\'', '"'])
                    .to_ascii_lowercase()
                    .replace([' ', '_'], "-");
                matches!(value.as_str(), "active-collaborator" | "close-contact")
                    .then(|| line.trim().to_string())
            });
            people.push(Person {
                slug: person.slug.clone(),
                emails: person.identities.email.clone(),
                relationship,
            });
        }
        people.sort_by(|a, b| a.slug.cmp(&b.slug));
        Ok(Self { people })
    }
    pub fn identities(&self, slug: &str) -> Option<&[String]> {
        self.people
            .iter()
            .find(|p| p.slug == slug)
            .map(|p| p.emails.as_slice())
    }
}
#[derive(Debug)]
pub enum Gate {
    Legacy,
    Muted,
    Priority { selector: String, level: Urgency },
    Relationship { request: String, line: String },
}
/// Only unambiguous concrete request forms, never questions such as 'can you
/// believe'. The exact source line is kept as evidence, not reconstructed prose.
pub fn concrete_request(body: &str) -> Option<String> {
    static REQUEST: OnceLock<regex::Regex> = OnceLock::new();
    let pattern=REQUEST.get_or_init(||regex::Regex::new(r"(?i)\b(?:(?:can|could|would|will)\s+you\s+(?:please\s+)?|please\s+|need\s+you\s+to\s+|would\s+you\s+mind\s+)(?:send|share|review|approve|confirm|provide|sign|reply|choose|decide|book|schedule|check|update|call|take|prepare|finish|look)\b").expect("constant request expression"));
    body.lines()
        .map(str::trim)
        .find(|line| line.len() >= 12 && pattern.is_match(line))
        .map(|s| s.chars().take(400).collect())
}
pub fn classify(
    sender: &str,
    body: &str,
    priorities: &[(String, Urgency)],
    catalog: &Catalog,
) -> Gate {
    let sender = bare_sender(sender);
    let people: Vec<_> = catalog
        .people
        .iter()
        .filter(|p| p.emails.iter().any(|e| e.eq_ignore_ascii_case(&sender)))
        .collect();
    let slugs: Vec<_> = people.iter().map(|p| p.slug.clone()).collect();
    let priority = priorities
        .iter()
        .filter(|(s, _)| selector_matches(s, &sender, &slugs))
        .max_by_key(|(s, u)| {
            let specificity = if s.starts_with("domain:") {
                1
            } else if s.starts_with("person:") {
                2
            } else {
                3
            };
            let level = match u {
                Urgency::Routine => 3,
                Urgency::Critical => 2,
                Urgency::High => 1,
            };
            (specificity, level)
        });
    if let Some((selector, level)) = priority {
        return if *level == Urgency::Routine {
            Gate::Muted
        } else {
            Gate::Priority {
                selector: selector.clone(),
                level: *level,
            }
        };
    }
    if people.len() == 1 {
        if let (Some(line), Some(request)) = (&people[0].relationship, concrete_request(body)) {
            return Gate::Relationship {
                request,
                line: format!("people/{}.md: {}", people[0].slug, line),
            };
        }
    }
    Gate::Legacy
}
pub fn plan(email: &Email, gate: &Gate, _now: i64) -> anyhow::Result<Option<AlertPlan>> {
    match gate {
        Gate::Priority { selector, level } => {
            let mut plan = AlertPlan {
                action: format!(
                    "Read email from {}",
                    email.from.chars().take(120).collect::<String>()
                ),
                reason: String::new(),
                evidence: String::new(),
                urgency: *level,
                due_at_ms: None,
                deadline_kind: "unknown".into(),
                meeting_id: None,
                meeting_start_ms: None,
                meeting_url: None,
                reply_resolves: true,
            };
            plan.reason = format!("Owner priority: {selector}");
            plan.evidence = plan.reason.clone();
            Ok(Some(plan))
        }
        Gate::Relationship { request, line } => Ok(Some(AlertPlan {
            action: request.clone(),
            reason: "Concrete request from a documented relationship".into(),
            evidence: format!("{request}\n{line}"),
            urgency: Urgency::High,
            due_at_ms: None,
            deadline_kind: "unknown".into(),
            meeting_id: None,
            meeting_start_ms: None,
            meeting_url: None,
            reply_resolves: true,
        })),
        _ => Ok(None),
    }
}
pub fn arrival(
    store: &Store,
    email: &Email,
    wiki: Option<&Path>,
    now: i64,
) -> anyhow::Result<Option<String>> {
    if !enabled() {
        return Ok(None);
    }
    let catalog = Catalog::load(wiki)?;
    let gate = classify(
        &email.from,
        &email.body,
        &store.owner_alert_priorities()?,
        &catalog,
    );
    let Some(plan) = plan(email, &gate, now)? else {
        return Ok(None);
    };
    let delay = if matches!(gate, Gate::Priority { .. }) {
        0
    } else {
        std::env::var("AUGMENTAGENT_ALERT_TEXT_DELAY_SECS")
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|s| (0..=86400).contains(s))
            .unwrap_or(600)
            * 1000
    };
    Ok(Some(plan.persist(
        store,
        email,
        &super::owner_alerts::timezone(),
        now,
        delay,
    )?))
}
