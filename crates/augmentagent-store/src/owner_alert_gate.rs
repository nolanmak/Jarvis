//! Owner-selected email/person/domain priorities, persisted without rebuilding.
use crate::{
    owner_alerts::Urgency,
    rusqlite::{params, Transaction, TransactionBehavior},
    Store, StoreError, StoreResult,
};

pub fn canonical_selector(value: &str) -> StoreResult<String> {
    let value = value.trim().to_ascii_lowercase();
    let invalid =
        || StoreError::InvalidInput("expected a bare email, person slug or exact domain".into());
    if value.is_empty() || value.len() > 254 || value.contains(char::is_whitespace) {
        return Err(invalid());
    }
    if let Some(slug) = value.strip_prefix("person:") {
        return if !slug.is_empty()
            && slug
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            Ok(value)
        } else {
            Err(invalid())
        };
    }
    if !value.starts_with('@') && !value.starts_with("domain:") && value.contains('@') {
        let parts: Vec<_> = value.split('@').collect();
        if parts.len() == 2
            && !parts[0].is_empty()
            && valid_domain(parts[1])
            && !value.contains(['<', '>', '\0'])
        {
            return Ok(value);
        }
        return Err(invalid());
    }
    let domain = value
        .strip_prefix("domain:")
        .or_else(|| value.strip_prefix('@'))
        .unwrap_or(&value);
    if domain.contains('.') && valid_domain(domain) {
        return Ok(format!("domain:{domain}"));
    }
    if value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Ok(format!("person:{value}"));
    }
    Err(invalid())
}
fn valid_domain(value: &str) -> bool {
    !value.is_empty()
        && value.split('.').all(|p| {
            !p.is_empty()
                && !p.starts_with('-')
                && !p.ends_with('-')
                && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}
pub fn bare_sender(value: &str) -> String {
    value
        .rsplit_once('<')
        .map(|(_, tail)| tail.split('>').next().unwrap_or(tail))
        .unwrap_or(value)
        .trim()
        .to_ascii_lowercase()
}
pub fn selector_matches(selector: &str, sender: &str, person_slugs: &[String]) -> bool {
    if let Some(slug) = selector.strip_prefix("person:") {
        person_slugs.iter().any(|s| s == slug)
    } else if let Some(domain) = selector.strip_prefix("domain:") {
        sender.rsplit_once('@').is_some_and(|(_, d)| d == domain)
    } else {
        selector.eq_ignore_ascii_case(sender)
    }
}
pub fn urgency(value: &str) -> Urgency {
    match value {
        "high" => Urgency::High,
        "critical" => Urgency::Critical,
        _ => Urgency::Routine,
    }
}

pub(crate) fn set_priority(
    store: &Store,
    selector: &str,
    level: Urgency,
    identities: &[String],
) -> StoreResult<()> {
    let selector = canonical_selector(selector)?;
    store.with_conn(|c| {
        let tx = Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO owner_alert_priorities(sender,urgency) VALUES(?1,?2)
            ON CONFLICT(sender) DO UPDATE SET urgency=excluded.urgency",
            params![selector, level.as_str()],
        )?;
        let messages = {
            let mut q = tx.prepare("SELECT messageId,fromEmail FROM emails")?;
            let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for (id, from) in messages {
            let sender = bare_sender(&from);
            if selector_matches(&selector, &sender, &[])
                || identities.iter().any(|i| i.eq_ignore_ascii_case(&sender))
            {
                tx.execute(
                    "DELETE FROM owner_alert_assessments WHERE message_id=?1",
                    [&id],
                )?;
                tx.execute(
                    "DELETE FROM owner_alert_assessment_failures WHERE message_id=?1",
                    [&id],
                )?;
            }
        }
        tx.commit()
    })?;
    Ok(())
}
impl Store {
    pub fn owner_alert_priorities(&self) -> StoreResult<Vec<(String, Urgency)>> {
        self.with_conn(|c| {
            let mut q =
                c.prepare("SELECT sender,urgency FROM owner_alert_priorities ORDER BY sender")?;
            let rows = q.query_map([], |r| Ok((r.get(0)?, urgency(&r.get::<_, String>(1)?))))?;
            rows.collect()
        })
    }
    pub fn set_owner_alert_priority_identities(
        &self,
        selector: &str,
        level: Urgency,
        identities: &[String],
    ) -> StoreResult<()> {
        set_priority(self, selector, level, identities)
    }
}
