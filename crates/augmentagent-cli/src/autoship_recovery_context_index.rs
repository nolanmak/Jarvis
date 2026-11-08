//! Physical recovery-context index migration. No recovery or execution grant.
use super::*;

const LEGACY: &str = "CREATE TABLE workflow_owner_recovery_native_intent (
    workflow TEXT NOT NULL REFERENCES workflow(id),request_id TEXT NOT NULL,transfer_id TEXT NOT NULL,
    generation INTEGER NOT NULL,pending_sequence INTEGER NOT NULL UNIQUE REFERENCES workflow_event(seq),
    sequence INTEGER NOT NULL UNIQUE REFERENCES workflow_event(seq),record_sha256 TEXT NOT NULL,record TEXT NOT NULL,
    PRIMARY KEY(workflow,request_id),UNIQUE(workflow,transfer_id),UNIQUE(workflow,generation))";

const CURRENT: &str = "CREATE TABLE workflow_owner_recovery_native_intent (
    workflow TEXT NOT NULL REFERENCES workflow(id),request_id TEXT NOT NULL,transfer_id TEXT NOT NULL,
    generation INTEGER NOT NULL,pending_sequence INTEGER NOT NULL UNIQUE REFERENCES workflow_event(seq),
    sequence INTEGER NOT NULL UNIQUE REFERENCES workflow_event(seq),record_sha256 TEXT NOT NULL,record TEXT NOT NULL,
    PRIMARY KEY(workflow,generation),UNIQUE(workflow,transfer_id))";
fn normalized(sql:&str)->String {
    let sql=sql.replace("\"workflow_owner_recovery_native_intent\"","workflow_owner_recovery_native_intent").to_ascii_lowercase();
    let mut words:Vec<_>=sql.split_ascii_whitespace().collect();
    if words.starts_with(&["create","table","if","not","exists"]) {words.drain(2..5);}
    // Preserve token boundaries: TEXT and T EXT are different SQLite types.
    // Never strip keyword-shaped substrings from column names or constraints.
    words.join(" ")
}
fn require_space(free:u64,reserved:u64,peak:u64,floor:u64)->Result<()> {
    ensure!(free.checked_sub(reserved).and_then(|n|n.checked_sub(peak)).is_some_and(|n|n>=floor),
        "native recovery index migration needs protected storage headroom");Ok(())
}
/// Called inside the original BEGIN IMMEDIATE. The caller retains the allocation
/// claim through COMMIT or connection-close rollback, including error returns.
/// Existing JSON/event bytes are never backfilled.
pub(super) fn migrate(conn:&Connection,path:&Path,space:&mut Option<crate::storage_reservations::Reservation>)->Result<()> {
    ensure!(!conn.is_autocommit(),"recovery index migration requires the schema transaction");
    let sql:String=conn.query_row("SELECT sql FROM sqlite_master WHERE name='workflow_owner_recovery_native_intent' AND type='table'",[],|r|r.get(0))?;
    if normalized(&sql)==normalized(CURRENT) {return Ok(());}
    ensure!(normalized(&sql)==normalized(LEGACY),"unsupported native recovery context index schema");
    let extensions:i64=conn.query_row("SELECT count(*) FROM sqlite_master WHERE lower(tbl_name)='workflow_owner_recovery_native_intent' AND type IN ('trigger','index') AND sql IS NOT NULL",[],|r|r.get(0))?;
    ensure!(extensions==0,"native recovery context index has extensions; retain original schema");
    let incoming:i64=conn.query_row("SELECT count(*) FROM sqlite_master m JOIN pragma_foreign_key_list(m.name) f WHERE m.type='table' AND lower(f.\"table\")='workflow_owner_recovery_native_intent'",[],|r|r.get(0))?;
    ensure!(incoming==0,"native recovery context index has dependent tables; retain original schema");
    let pages:u64=conn.query_row("PRAGMA page_count",[],|r|r.get(0))?;
    let page_size:u64=conn.query_row("PRAGMA page_size",[],|r|r.get(0))?;
    // Conservatively cover a complete additional database image plus indexes
    // and WAL growth, rather than estimating from the visible context payloads.
    let peak=pages.checked_mul(page_size).and_then(|n|n.checked_mul(4)).and_then(|n|n.checked_add(1024*1024))
        .context("recovery index migration storage bound overflow")?;
    *space=Some({
        let admission=crate::storage_reservations::Admission::lock()?;
        let reserved=admission.claims(None)?.reserved_on(path)?;
        let free=crate::disk::available_bytes(path).context("recovery index migration filesystem unavailable")?;
        let floor=(crate::disk::gate_min_free_gb()*1024.0_f64.powi(3)) as u64;
        require_space(free,reserved,peak,floor)?;
        let mut space=crate::storage_reservations::Reservation::new(&admission,path,false)?;
        space.publish(&admission,&[path.to_path_buf()],peak)?;
        space
    });
    conn.execute_batch(&CURRENT.replace("CREATE TABLE workflow_owner_recovery_native_intent","CREATE TABLE workflow_owner_recovery_native_intent_migration"))?;
    conn.execute_batch("INSERT INTO workflow_owner_recovery_native_intent_migration SELECT * FROM workflow_owner_recovery_native_intent;
        DROP TABLE workflow_owner_recovery_native_intent;
        ALTER TABLE workflow_owner_recovery_native_intent_migration RENAME TO workflow_owner_recovery_native_intent;")?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn install_legacy_fixture(conn:&Connection)->Result<()> {
    conn.execute_batch(&LEGACY.replace("CREATE TABLE workflow_owner_recovery_native_intent","CREATE TABLE workflow_owner_recovery_native_intent_legacy_fixture"))?;
    conn.execute_batch("INSERT INTO workflow_owner_recovery_native_intent_legacy_fixture SELECT * FROM workflow_owner_recovery_native_intent;
        DROP TABLE workflow_owner_recovery_native_intent;
        ALTER TABLE workflow_owner_recovery_native_intent_legacy_fixture RENAME TO workflow_owner_recovery_native_intent;")?;Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migration_admission_keeps_the_configured_floor_and_other_writers_reserved() {
        assert!(require_space(100,20,30,50).is_ok());
        assert!(require_space(99,20,30,50).is_err());
        assert!(require_space(100,u64::MAX,30,50).is_err());
        assert!(require_space(100,20,u64::MAX,50).is_err());
        assert!(require_space(1,0,1,0).is_ok());
        assert!(require_space(0,0,1,0).is_err(),"zero floor cannot waive actual copy demand");
        assert!(require_space(0,1,0,0).is_err(),"zero floor cannot waive another writer's reservation");
    }
    #[test]
    fn failed_copy_retains_its_allocation_claim_until_connection_rollback() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");let conn=legacy(&path);
        conn.pragma_update(None,"foreign_keys",false).unwrap();
        conn.execute("UPDATE workflow_owner_recovery_native_intent SET pending_sequence=999999",[]).unwrap();
        conn.pragma_update(None,"foreign_keys",true).unwrap();
        let old_schema=schema(&conn);let old_rows=retained(&conn);
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        let mut space=None;
        assert!(migrate(&conn,&path,&mut space).is_err());
        assert!(space.is_some(),"failed copy dropped its in-flight allocation claim");
        assert!(!conn.is_autocommit(),"copy failure unexpectedly escaped the schema transaction");
        {
            let admission=crate::storage_reservations::Admission::lock().unwrap();
            assert!(admission.claims(None).unwrap().reserved_on(&path).unwrap()>=1024*1024);
        }
        // This is also the writable opener's error ordering: SQLite closes and
        // rolls back before the still-live allocation claim can be dropped.
        drop(conn);
        let original=Connection::open(&path).unwrap();
        assert_eq!(schema(&original),old_schema);
        assert_eq!(retained(&original),old_rows);
        drop(space);
    }
    #[test]
    fn process_exit_before_or_after_commit_preserves_original_context() {
        const CHILD:&str="JARVIS_TEST_RECOVERY_INDEX_EXIT";
        if let Some(path)=std::env::var_os(CHILD) {
            let conn=Connection::open(&path).unwrap();
            conn.pragma_update(None,"foreign_keys",true).unwrap();
            conn.execute_batch("BEGIN IMMEDIATE").unwrap();
            let mut space=None;migrate(&conn,Path::new(&path),&mut space).unwrap();
            assert!(space.is_some());
            if std::env::var("JARVIS_TEST_RECOVERY_INDEX_COMMITTED").unwrap()=="1" {conn.execute_batch("COMMIT").unwrap();}
            // Abrupt process loss, without SQLite/claim Rust destructors or a
            // successful return/acknowledgement to the controlling parent.
            unsafe {libc::_exit(77)}
        }
        for committed in [false,true] {
            let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");let conn=legacy(&path);
            let before=retained(&conn);drop(conn);
            let output=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","autoship_workflow::recovery_context_index::tests::process_exit_before_or_after_commit_preserves_original_context","--nocapture"])
                .env(CHILD,&path).env("JARVIS_TEST_RECOVERY_INDEX_COMMITTED",if committed {"1"}else{"0"}).output().unwrap();
            assert_eq!(output.status.code(),Some(77),"{} {}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));
            let conn=Connection::open(&path).unwrap();
            assert_eq!(normalized(&schema(&conn)),normalized(if committed {CURRENT}else{LEGACY}));
            assert_eq!(retained(&conn),before);drop(conn);
            let reopened=WorkflowStore::open(&path).unwrap();
            assert_eq!(key(&reopened.conn),["workflow","generation"]);
            assert_eq!(retained(&reopened.conn),before);
        }
    }
    fn key(conn:&Connection)->Vec<String> {
        conn.prepare("SELECT name FROM pragma_table_info('workflow_owner_recovery_native_intent') WHERE pk>0 ORDER BY pk").unwrap()
            .query_map([],|r|r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
    }
    fn retained(conn:&Connection)->Vec<String> {
        conn.prepare("SELECT json_array(workflow,request_id,transfer_id,generation,pending_sequence,sequence,record_sha256,record) FROM workflow_owner_recovery_native_intent").unwrap()
            .query_map([],|r|r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
    }
    fn schema(conn:&Connection)->String {
        conn.query_row("SELECT sql FROM sqlite_master WHERE name='workflow_owner_recovery_native_intent'",[],|r|r.get(0)).unwrap()
    }
    fn legacy(path:&Path)->Connection {
        let mut store=WorkflowStore::open(path).unwrap();
        store.register("fixture","fixture",Binding {head:"head".into(),contract:"contract".into(),evidence_hash:"evidence".into()},0).unwrap();
        let conn=store.conn;
        conn.execute_batch("DROP TABLE workflow_owner_recovery_native_intent").unwrap();
        conn.execute_batch(LEGACY).unwrap();
        conn.execute("INSERT INTO workflow_event(workflow,generation,kind,payload,at_ms) VALUES('fixture',3,'native_handoff_prepared','opaque original pending',1)",[]).unwrap();
        let pending=conn.last_insert_rowid();
        conn.execute("INSERT INTO workflow_event(workflow,generation,kind,payload,at_ms) VALUES('fixture',3,'owner_recovery_native_intent_recorded','opaque original event',1)",[]).unwrap();
        let sequence=conn.last_insert_rowid();
        // Opaque migration bytes, deliberately not an authenticated context.
        conn.execute("INSERT INTO workflow_owner_recovery_native_intent VALUES('fixture','request','transfer',3,?1,?2,?3,?4)",
            params![pending,sequence,"a".repeat(64),"{ \"version\": 1, \"original\": \"retain exact bytes\" }"]).unwrap();
        conn
    }
    #[test]
    fn legacy_context_index_migrates_verbatim_and_reopens_idempotently() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");let conn=legacy(&path);
        let before=retained(&conn);
        let events:String=conn.query_row("SELECT group_concat(payload,'|') FROM workflow_event",[],|r|r.get(0)).unwrap();
        for _ in 0..2 {
            let reopened=WorkflowStore::open(&path).unwrap();
            assert_eq!(key(&reopened.conn),["workflow","generation"]);
            assert_eq!(retained(&reopened.conn),before);
            assert_eq!(reopened.conn.query_row("SELECT group_concat(payload,'|') FROM workflow_event",[],|r|r.get::<_,String>(0)).unwrap(),events);
            assert_eq!(reopened.conn.query_row("PRAGMA foreign_keys",[],|r|r.get::<_,i64>(0)).unwrap(),1);
        }
    }
    #[test]
    fn legacy_context_migration_refuses_extensions_and_damaged_references_atomically() {
        for fault in ["trigger","index","incoming-cascade","missing-event","shape","type-token-collision","trigger-case","incoming-cascade-case"] {
            let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");let conn=legacy(&path);
            match fault {
                "trigger"=>conn.execute_batch("CREATE TRIGGER owner_extension BEFORE DELETE ON workflow_owner_recovery_native_intent BEGIN SELECT RAISE(ABORT,'preserve'); END").unwrap(),
                "trigger-case"=>conn.execute_batch("CREATE TRIGGER owner_extension BEFORE DELETE ON WORKFLOW_OWNER_RECOVERY_NATIVE_INTENT BEGIN SELECT RAISE(ABORT,'preserve'); END").unwrap(),
                "index"=>conn.execute_batch("CREATE INDEX owner_extension ON workflow_owner_recovery_native_intent(record_sha256)").unwrap(),
                "incoming-cascade"=>conn.execute_batch("CREATE TABLE owner_extension(context INTEGER REFERENCES workflow_owner_recovery_native_intent(sequence) ON DELETE CASCADE); INSERT INTO owner_extension SELECT sequence FROM workflow_owner_recovery_native_intent").unwrap(),
                "incoming-cascade-case"=>conn.execute_batch("CREATE TABLE owner_extension(context INTEGER REFERENCES WORKFLOW_OWNER_RECOVERY_NATIVE_INTENT(sequence) ON DELETE CASCADE); INSERT INTO owner_extension SELECT sequence FROM workflow_owner_recovery_native_intent").unwrap(),
                "missing-event"=>{conn.pragma_update(None,"foreign_keys",false).unwrap();conn.execute("UPDATE workflow_owner_recovery_native_intent SET pending_sequence=999999",[]).unwrap();},
                "shape"=>conn.execute_batch("ALTER TABLE workflow_owner_recovery_native_intent ADD COLUMN owner_extension TEXT").unwrap(),
                "type-token-collision"=>{
                    conn.execute_batch(&LEGACY.replace("CREATE TABLE workflow_owner_recovery_native_intent","CREATE TABLE context_fixture")
                        .replace("record TEXT NOT NULL","record T EXT NOT NULL")).unwrap();
                    conn.execute_batch("INSERT INTO context_fixture SELECT * FROM workflow_owner_recovery_native_intent; DROP TABLE workflow_owner_recovery_native_intent; ALTER TABLE context_fixture RENAME TO workflow_owner_recovery_native_intent").unwrap();
                },
                _=>unreachable!(),
            }
            let old=retained(&conn);let original_schema=schema(&conn);
            assert!(WorkflowStore::open(&path).is_err(),"{fault}: unsupported legacy index migrated");
            assert_eq!(schema(&conn),original_schema,"{fault}: failed migration changed schema");
            assert_eq!(retained(&conn),old,"{fault}: failed migration changed original bytes");
            if fault.starts_with("incoming-cascade") {assert_eq!(conn.query_row("SELECT count(*) FROM owner_extension",[],|r|r.get::<_,i64>(0)).unwrap(),1);}
        }
    }
}
