use augmentagent_messages::people::{resolve_person, MatchKind};
use augmentagent_store::Store;

fn kind_name(kind: MatchKind) -> &'static str {
    match kind {
        MatchKind::Handle => "handle",
        MatchKind::Exact => "exact",
        MatchKind::Partial => "partial",
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::temp_dir().join(format!(
        "autoship-1668-{}.db",
        std::process::id(),
    ));
    let _ = std::fs::remove_file(&path);
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
            let entries = matches
                .into_iter()
                .map(|candidate| format!("[\"{}\",\"{}\"]", candidate.key, kind_name(candidate.kind)))
                .collect::<Vec<_>>();
            cases.push(format!("[{}]", entries.join(",")));
        }
        Ok::<_, augmentagent_store::rusqlite::Error>(cases)
    })?;
    println!("[{}]", observed.join(","));
    let _ = std::fs::remove_file(path);
    Ok(())
}
