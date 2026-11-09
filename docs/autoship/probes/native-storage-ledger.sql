-- Proposed native balance schema only. No production migration/authority.
CREATE TABLE capacity_meta (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 version INTEGER NOT NULL CHECK(version=1),
 root_uuid BLOB NOT NULL UNIQUE CHECK(length(root_uuid)=16),
 state_filesystem_uuid BLOB NOT NULL CHECK(length(state_filesystem_uuid)=16),
 revision INTEGER NOT NULL CHECK(revision>=0)
) STRICT;
CREATE TABLE capacity_operations (
 operation_uuid BLOB PRIMARY KEY CHECK(length(operation_uuid)=16),
 root_uuid BLOB NOT NULL REFERENCES capacity_meta(root_uuid),
 session_uuid BLOB NOT NULL CHECK(length(session_uuid)=16),
 launch_uuid BLOB NOT NULL UNIQUE CHECK(length(launch_uuid)=16),
 action TEXT NOT NULL CHECK(action IN ('create','resume')),
 origin BLOB NOT NULL CHECK(length(origin) BETWEEN 1 AND 65536),
 policy_sha BLOB NOT NULL CHECK(length(policy_sha)=32),
 demand_sha BLOB NOT NULL CHECK(length(demand_sha)=32),
 replay_sha BLOB NOT NULL CHECK(length(replay_sha)=32),
 UNIQUE(operation_uuid,session_uuid)
) STRICT, WITHOUT ROWID;
CREATE TABLE capacity_allocations (
 allocation_uuid BLOB PRIMARY KEY CHECK(length(allocation_uuid)=16),
 operation_uuid BLOB NOT NULL,
 session_uuid BLOB NOT NULL CHECK(length(session_uuid)=16),
 kind TEXT NOT NULL CHECK(kind IN ('retained','active-growth')),
 demand_count INTEGER NOT NULL CHECK(demand_count BETWEEN 1 AND 64),
 phase TEXT NOT NULL CHECK(phase IN ('pending','held','release-pending','released')),
 release_sha BLOB CHECK(release_sha IS NULL OR length(release_sha)=32),
 FOREIGN KEY(operation_uuid,session_uuid) REFERENCES capacity_operations(operation_uuid,session_uuid),
 UNIQUE(operation_uuid,kind),
 CHECK(kind!='retained' OR demand_count=1),
 CHECK((phase='released' AND release_sha IS NOT NULL) OR
       (phase!='released' AND release_sha IS NULL))
) STRICT, WITHOUT ROWID;
CREATE UNIQUE INDEX capacity_one_retained ON capacity_allocations(session_uuid)
 WHERE kind='retained';
CREATE TABLE capacity_demands (
 allocation_uuid BLOB NOT NULL REFERENCES capacity_allocations(allocation_uuid),
 filesystem_uuid BLOB NOT NULL CHECK(length(filesystem_uuid)=16),
 bytes INTEGER NOT NULL CHECK(bytes>=0),
 inodes INTEGER NOT NULL CHECK(inodes>=0),
 PRIMARY KEY(allocation_uuid,filesystem_uuid),
 CHECK(bytes>0 OR inodes>0)
) STRICT, WITHOUT ROWID;
CREATE TRIGGER capacity_meta_identity BEFORE UPDATE OF singleton,version,root_uuid,state_filesystem_uuid ON capacity_meta
 BEGIN SELECT RAISE(ABORT,'immutable capacity identity'); END;
CREATE TRIGGER capacity_meta_delete BEFORE DELETE ON capacity_meta
 BEGIN SELECT RAISE(ABORT,'capacity identity is retained'); END;
CREATE TRIGGER capacity_meta_revision BEFORE UPDATE OF revision ON capacity_meta
 WHEN NEW.revision!=OLD.revision+1 OR typeof(NEW.revision)!='integer'
 BEGIN SELECT RAISE(ABORT,'invalid capacity revision'); END;
CREATE TRIGGER capacity_operations_update BEFORE UPDATE ON capacity_operations
 BEGIN SELECT RAISE(ABORT,'immutable capacity operation'); END;
CREATE TRIGGER capacity_operations_delete BEFORE DELETE ON capacity_operations
 BEGIN SELECT RAISE(ABORT,'capacity operation is retained'); END;
CREATE TRIGGER capacity_allocations_identity BEFORE UPDATE OF allocation_uuid,operation_uuid,session_uuid,kind,demand_count ON capacity_allocations
 BEGIN SELECT RAISE(ABORT,'immutable capacity allocation'); END;
CREATE TRIGGER capacity_allocations_delete BEFORE DELETE ON capacity_allocations
 BEGIN SELECT RAISE(ABORT,'capacity allocation is retained'); END;
CREATE TRIGGER capacity_allocations_phase BEFORE UPDATE OF phase,release_sha ON capacity_allocations
 WHEN NOT (
   (OLD.phase=NEW.phase AND OLD.release_sha IS NEW.release_sha) OR
   (OLD.phase='pending' AND NEW.phase IN ('held','release-pending') AND NEW.release_sha IS NULL) OR
   (OLD.phase='held' AND NEW.phase='release-pending' AND NEW.release_sha IS NULL) OR
   (OLD.phase='release-pending' AND NEW.phase='released' AND NEW.release_sha IS NOT NULL))
 BEGIN SELECT RAISE(ABORT,'invalid capacity transition'); END;
CREATE TRIGGER capacity_demands_update BEFORE UPDATE ON capacity_demands
 BEGIN SELECT RAISE(ABORT,'immutable capacity demand'); END;
CREATE TRIGGER capacity_demands_delete BEFORE DELETE ON capacity_demands
 BEGIN SELECT RAISE(ABORT,'capacity demand is retained'); END;
CREATE TRIGGER capacity_allocations_initial BEFORE INSERT ON capacity_allocations
 WHEN NEW.phase!='pending' OR NEW.release_sha IS NOT NULL
 BEGIN SELECT RAISE(ABORT,'initial capacity state must be pending'); END;
CREATE TRIGGER capacity_retained_demand BEFORE INSERT ON capacity_demands
 WHEN (SELECT kind FROM capacity_allocations WHERE allocation_uuid=NEW.allocation_uuid)='retained'
 AND (NEW.filesystem_uuid!=(SELECT state_filesystem_uuid FROM capacity_meta WHERE singleton=1)
      OR NEW.bytes!=1073741824)
 BEGIN SELECT RAISE(ABORT,'retained demand must bind state filesystem'); END;
CREATE TRIGGER capacity_retained_release BEFORE UPDATE OF phase ON capacity_allocations
 WHEN NEW.kind='retained' AND NEW.phase IN ('release-pending','released')
 AND EXISTS(SELECT 1 FROM capacity_allocations WHERE session_uuid=NEW.session_uuid
            AND kind='active-growth' AND phase!='released')
 BEGIN SELECT RAISE(ABORT,'active launches prevent retained release'); END;
CREATE TRIGGER capacity_resume_retained BEFORE INSERT ON capacity_allocations
 WHEN NEW.kind='active-growth'
 AND (SELECT action FROM capacity_operations WHERE operation_uuid=NEW.operation_uuid)='resume'
 AND NOT EXISTS(SELECT 1 FROM capacity_allocations WHERE session_uuid=NEW.session_uuid
                AND kind='retained' AND phase='held')
 BEGIN SELECT RAISE(ABORT,'resume requires held retained allocation'); END;
CREATE TRIGGER capacity_meta_collision BEFORE INSERT ON capacity_meta
 WHEN EXISTS(SELECT 1 FROM capacity_meta)
 BEGIN SELECT RAISE(ABORT,'duplicate capacity identity'); END;
CREATE TRIGGER capacity_operations_collision BEFORE INSERT ON capacity_operations
 WHEN EXISTS(SELECT 1 FROM capacity_operations
             WHERE operation_uuid=NEW.operation_uuid OR launch_uuid=NEW.launch_uuid)
 BEGIN SELECT RAISE(ABORT,'duplicate capacity operation'); END;
CREATE TRIGGER capacity_allocations_collision BEFORE INSERT ON capacity_allocations
 WHEN EXISTS(SELECT 1 FROM capacity_allocations
             WHERE allocation_uuid=NEW.allocation_uuid
             OR (operation_uuid=NEW.operation_uuid AND kind=NEW.kind)
             OR (kind='retained' AND NEW.kind='retained' AND session_uuid=NEW.session_uuid))
 BEGIN SELECT RAISE(ABORT,'duplicate capacity allocation'); END;
CREATE TRIGGER capacity_demands_collision BEFORE INSERT ON capacity_demands
 WHEN EXISTS(SELECT 1 FROM capacity_demands
             WHERE allocation_uuid=NEW.allocation_uuid AND filesystem_uuid=NEW.filesystem_uuid)
 BEGIN SELECT RAISE(ABORT,'duplicate capacity demand'); END;
