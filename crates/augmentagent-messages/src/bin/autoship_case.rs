use augmentagent_messages::people::{resolve_person, MatchKind};
use augmentagent_store::Store;

fn kind_name(kind: MatchKind) -> String {
    match kind {
        MatchKind::Handle => "handle".to_string(),
        MatchKind::Exact => "exact".to_string(),
        MatchKind::Partial => "partial".to_string(),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::temp_dir().join(format!(
        "autoship-1668-{}-{}.db",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let store = Store::open(&path)?;
    store.with_conn(|c| {
        c.execute(
            "INSERT INTO message_person_names (person_key, name) VALUES (?1, ?2)",
            ["jordan", "jordan"],
        )?;
        c.execute(
            "INSERT INTO message_person_names (person_key, name) VALUES (?1, ?2)",
            ["jordan-blake", "jordan blake"],
        )?;
        Ok::<(), augmentagent_store::rusqlite::Error>(())
    })?;
    let observed = store.with_conn(|c| {
        let mut cases = Vec::new();
        for query in ["jordan", "Jordan Blake", "jordan-blake"] {
            let matches = resolve_person(c, query)?;
            cases.push(
                matches
                    .into_iter()
                    .map(|candidate| vec![candidate.key, kind_name(candidate.kind)])
                    .collect::<Vec<_>>(),
            );
        }
        Ok::<_, augmentagent_store::rusqlite::Error>(cases)
    })?;
    println!("{}", serde_json::to_string(&observed)?);
    Ok(())
}
