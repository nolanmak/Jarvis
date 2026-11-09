#!/usr/bin/env python3
"""Disposable SQL-contract experiment; never opens a caller-supplied database."""
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile

SCHEMA = Path(__file__).with_name('native-storage-ledger.sql').read_text()
MAX = (1 << 63) - 1
GIB = 1 << 30
ROOT, SESSION, LAUNCH, OP, RETAINED, GROWTH, FS = [bytes([i]) * 16 for i in range(1, 8)]
ORIGIN = b'synthetic-original-binding-not-authority'


def connect(path):
    db = sqlite3.connect(path, isolation_level=None, timeout=0)
    db.execute('PRAGMA foreign_keys=ON')
    db.execute('PRAGMA synchronous=FULL')
    assert db.execute('PRAGMA foreign_keys').fetchone()[0] == 1
    assert db.execute('PRAGMA synchronous').fetchone()[0] == 2
    assert db.execute('PRAGMA busy_timeout').fetchone()[0] == 0
    return db


def payload():
    return {'version': 1, 'root': ROOT.hex(), 'operation': OP.hex(),
            'session': SESSION.hex(), 'launch': LAUNCH.hex(), 'action': 'create',
            'origin_sha': hashlib.sha256(ORIGIN).hexdigest(), 'policy_sha': '11' * 32,
            'demand_sha': '22' * 32,
            'allocations': [
                {'id': RETAINED.hex(), 'kind': 'retained',
                 'demands': [{'filesystem': FS.hex(), 'bytes': GIB, 'inodes': 16}]},
                {'id': GROWTH.hex(), 'kind': 'active-growth',
                 'demands': [{'filesystem': FS.hex(), 'bytes': 2 * GIB, 'inodes': 32}]}]}


def replay(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':'),
                                    ensure_ascii=True).encode()).digest()


def insert(db, p=None, omit_demand=False, digest_override=None):
    p = payload() if p is None else p
    db.execute('INSERT INTO capacity_operations VALUES(?,?,?,?,?,?,?,?,?)',
               (bytes.fromhex(p['operation']), bytes.fromhex(p['root']), bytes.fromhex(p['session']),
                bytes.fromhex(p['launch']), p['action'], ORIGIN, bytes.fromhex(p['policy_sha']),
                bytes.fromhex(p['demand_sha']), replay(p) if digest_override is None else digest_override))
    for row in p['allocations']:
        allocation = bytes.fromhex(row['id'])
        db.execute('INSERT INTO capacity_allocations VALUES(?,?,?,?,?,?,?)',
                   (allocation, bytes.fromhex(p['operation']), bytes.fromhex(p['session']), row['kind'], len(row['demands']), 'pending', None))
        for d in row['demands']:
            if not (omit_demand and row['kind'] == 'active-growth'):
                db.execute('INSERT INTO capacity_demands VALUES(?,?,?,?)',
                           (allocation, bytes.fromhex(d['filesystem']), d['bytes'], d['inodes']))
    revision = db.execute('SELECT revision FROM capacity_meta').fetchone()[0]
    assert db.execute('UPDATE capacity_meta SET revision=revision+1 WHERE revision=?', (revision,)).rowcount == 1


def signature(db):
    return db.execute("SELECT type,name,tbl_name,sql FROM sqlite_schema "
                      "WHERE name NOT GLOB 'sqlite_*' ORDER BY type,name").fetchall()


def initialize(db):
    db.execute('CREATE TABLE retired_owner(audit_path TEXT NOT NULL,workflow TEXT NOT NULL,generation INTEGER NOT NULL,PRIMARY KEY(audit_path,workflow,generation))')
    db.executescript('BEGIN IMMEDIATE;\n' + SCHEMA + '\nCOMMIT;')
    db.execute('INSERT INTO capacity_meta VALUES(1,1,?,?,0)', (ROOT, FS))


def inventory(db, expected_schema):
    if signature(db) != expected_schema or db.execute('PRAGMA foreign_key_check').fetchall():
        raise ValueError('unknown schema or graph')
    if db.execute('SELECT singleton,version,root_uuid,state_filesystem_uuid FROM capacity_meta').fetchall() != [(1, 1, ROOT, FS)]:
        raise ValueError('unknown root')
    totals = {}
    for op in db.execute('SELECT * FROM capacity_operations').fetchall():
        oid, root, session, launch, action, origin, policy, demand, digest = op
        rows = db.execute('SELECT allocation_uuid,kind,demand_count,phase,release_sha '
                          'FROM capacity_allocations WHERE operation_uuid=? '
                          "ORDER BY CASE kind WHEN 'retained' THEN 0 ELSE 1 END", (oid,)).fetchall()
        if [r[1] for r in rows] != (['retained', 'active-growth'] if action == 'create' else ['active-growth']):
            raise ValueError('incomplete operation')
        p = {'version': 1, 'root': root.hex(), 'operation': oid.hex(), 'session': session.hex(),
             'launch': launch.hex(), 'action': action, 'origin_sha': hashlib.sha256(origin).hexdigest(),
             'policy_sha': policy.hex(), 'demand_sha': demand.hex(), 'allocations': []}
        for aid, kind, count, phase, proof in rows:
            demands = db.execute('SELECT filesystem_uuid,bytes,inodes FROM capacity_demands '
                                 'WHERE allocation_uuid=? ORDER BY filesystem_uuid', (aid,)).fetchall()
            if len(demands) != count:
                raise ValueError('incomplete demand vector')
            if kind == 'retained' and (len(demands) != 1 or demands[0][0] != FS or demands[0][1] != GIB):
                raise ValueError('retained promise changed')
            if kind == 'active-growth':
                retained = db.execute("SELECT phase,operation_uuid FROM capacity_allocations "
                                      "WHERE session_uuid=? AND kind='retained'", (session,)).fetchone()
                if retained is None or (phase != 'released' and not (retained[0] == 'held' or
                        (retained == ('pending', oid) and action == 'create'))):
                    raise ValueError('active launch lacks retained promise')
            p['allocations'].append({'id': aid.hex(), 'kind': kind,
                                     'demands': [{'filesystem': f.hex(), 'bytes': b, 'inodes': n}
                                                 for f, b, n in demands]})
            if phase != 'released':
                for f, b, n in demands:
                    before = totals.get(f, (0, 0))
                    after = (before[0] + b, before[1] + n)
                    if max(after) > MAX:
                        raise ValueError('checked balance overflow')
                    totals[f] = after
        if replay(p) != digest:
            raise ValueError('replay changed')
    return totals


def main():
    checks = []
    with tempfile.TemporaryDirectory(prefix='autoship-native-ledger-') as scratch:
        root = Path(scratch)
        path = root / 'fixture.sqlite'
        db = connect(path)
        initialize(db)
        expected = signature(db)
        db.execute('BEGIN IMMEDIATE')
        insert(db)
        db.commit()
        assert inventory(db, expected)[FS] == (3 * GIB, 48)
        checks.append('full-pending-vector')

        def rejected(label, sql, args=(), message=None):
            db.execute('SAVEPOINT refusal')
            try:
                db.execute(sql, args)
            except (sqlite3.IntegrityError, sqlite3.DataError, OverflowError) as error:
                if message is not None:
                    assert message in str(error)
                checks.append(label)
            else:
                raise AssertionError(label + ' accepted')
            finally:
                db.execute('ROLLBACK TO refusal')
                db.execute('RELEASE refusal')

        rejected('terminal-insertion-refused', 'INSERT INTO capacity_allocations VALUES(?,?,?,?,?,?,?)',
                 (b'n' * 16, b'o' * 16, b's' * 16, 'active-growth', 1, 'released', b'p' * 32),
                 'initial capacity state must be pending')
        rejected('early-retained-release-refused', "UPDATE capacity_allocations SET phase='release-pending' WHERE allocation_uuid=?",
                 (RETAINED,), 'active launches prevent retained release')
        rejected('immutable-state-filesystem', 'UPDATE capacity_meta SET state_filesystem_uuid=?',
                 (b'w' * 16,), 'immutable capacity identity')
        rejected('immutable-demand', 'UPDATE capacity_demands SET bytes=0')
        rejected('immutable-origin', "UPDATE capacity_operations SET origin=x'00'")
        rejected('deletion-refused', 'DELETE FROM capacity_allocations')
        rejected('release-shortcut-refused', "UPDATE capacity_allocations SET phase='released',release_sha=?", (b'p' * 32,))
        rejected('unknown-phase-refused', "UPDATE capacity_allocations SET phase='invented'")
        rejected('revision-regression-refused', 'UPDATE capacity_meta SET revision=0')
        rejected('revision-jump-refused', 'UPDATE capacity_meta SET revision=3')
        rejected('duplicate-filesystem-refused', 'INSERT INTO capacity_demands VALUES(?,?,?,?)', (GROWTH, FS, 1, 1))
        rejected('unbound-demand-refused', 'INSERT INTO capacity_demands VALUES(?,?,?,?)', (b'z' * 16, FS, 1, 1))
        rejected('numeric-overflow-refused', 'INSERT INTO capacity_demands VALUES(?,?,?,?)', (GROWTH, b'z' * 16, 1 << 63, 1))
        rejected('negative-demand-refused', 'INSERT INTO capacity_demands VALUES(?,?,?,?)', (GROWTH, b'z' * 16, -1, 1))
        rejected('nonintegral-demand-refused', 'INSERT INTO capacity_demands VALUES(?,?,?,?)', (GROWTH, b'z' * 16, 1.5, 1))
        for phase in ('held', 'release-pending'):
            db.execute('UPDATE capacity_allocations SET phase=? WHERE allocation_uuid=?', (phase, GROWTH))
            assert inventory(db, expected)[FS] == (3 * GIB, 48)
            checks.append(phase + '-full-charge')
        # SQL validates shape only. This synthetic digest is deliberately NOT native proof.
        db.execute("UPDATE capacity_allocations SET phase='released',release_sha=? WHERE allocation_uuid=?", (b'p' * 32, GROWTH))
        assert inventory(db, expected)[FS] == (GIB, 16)
        checks.append('released-only-zero-shape-not-proof')
        rejected('replace-cannot-reopen-released-allocation', 'INSERT OR REPLACE INTO capacity_allocations VALUES(?,?,?,?,?,?,?)',
                 (GROWTH, OP, SESSION, 'active-growth', 1, 'pending', None), 'duplicate capacity allocation')
        rejected('replace-cannot-change-operation', 'INSERT OR REPLACE INTO capacity_operations VALUES(?,?,?,?,?,?,?,?,?)',
                 (OP, ROOT, SESSION, LAUNCH, 'create', b'changed', b'p' * 32, b'd' * 32, b'r' * 32),
                 'duplicate capacity operation')
        rejected('replace-cannot-change-demand', 'INSERT OR REPLACE INTO capacity_demands VALUES(?,?,?,?)',
                 (GROWTH, FS, 1, 1), 'duplicate capacity demand')
        rejected('replace-cannot-reset-root', 'INSERT OR REPLACE INTO capacity_meta VALUES(1,1,?,?,0)',
                 (ROOT, FS), 'duplicate capacity identity')
        rejected('released-row-cannot-reopen', "UPDATE capacity_allocations SET phase='held',release_sha=NULL WHERE allocation_uuid=?", (GROWTH,))
        changed = payload(); changed['origin_sha'] = 'ff' * 32
        assert replay(changed) != replay(payload())
        checks.append('changed-origin-changes-replay')
        db.close()
        db = connect(path)
        assert inventory(db, expected)[FS] == (GIB, 16)
        checks.append('committed-balance-survives-reopen')
        reader = sqlite3.connect(path.as_uri() + '?mode=ro', uri=True, timeout=0, isolation_level=None)
        reader.execute('PRAGMA query_only=ON')
        db.execute('BEGIN EXCLUSIVE')
        try:
            reader.execute('SELECT * FROM capacity_allocations').fetchall()
        except sqlite3.OperationalError as e:
            assert 'locked' in str(e)
            assert reader.execute('PRAGMA busy_timeout').fetchone()[0] == 0
            checks.append('busy-read-refuses-with-zero-wait')
        else:
            raise AssertionError('exclusive writer was ignored')
        db.rollback(); reader.close(); db.close()
        for label, filesystem, byte_count in [
            ('retained-wrong-filesystem-refused', (b'w' * 16).hex(), GIB),
            ('retained-wrong-byte-charge-refused', FS.hex(), GIB - 1),
        ]:
            guard = connect(root / (label + '.sqlite')); initialize(guard)
            document = payload()
            document['allocations'][0]['demands'][0].update(filesystem=filesystem, bytes=byte_count)
            guard.execute('BEGIN IMMEDIATE')
            try:
                insert(guard, document)
            except sqlite3.IntegrityError as error:
                assert 'retained demand must bind state filesystem' in str(error)
                checks.append(label)
            else:
                raise AssertionError(label + ' accepted')
            guard.rollback(); guard.close()
        for label, alter, options, reason in [
            ('missing-operation-child-is-unknown', lambda p: p['allocations'].pop(), {}, 'incomplete operation'),
            ('missing-vector-child-is-unknown', lambda p: None, {'omit_demand': True}, 'incomplete demand vector'),
            ('changed-replay-is-unknown', lambda p: None, {'digest_override': b'x' * 32}, 'replay changed'),
            ('aggregate-overflow-is-unknown', lambda p: p['allocations'][1]['demands'][0].update(bytes=MAX), {}, 'checked balance overflow'),
        ]:
            invalid = connect(root / (label + '.sqlite')); initialize(invalid)
            document = payload(); alter(document)
            invalid.execute('BEGIN IMMEDIATE'); insert(invalid, document, **options); invalid.commit()
            try:
                inventory(invalid, expected)
            except ValueError as error:
                assert str(error) == reason
                checks.append(label)
            else:
                raise AssertionError(label + ' accepted')
            invalid.close()
        unknown = connect(root / 'unknown-schema.sqlite'); initialize(unknown)
        unknown.execute('CREATE TABLE sqlitex_unexpected(value TEXT)')
        try:
            inventory(unknown, expected)
        except ValueError as error:
            assert str(error) == 'unknown schema or graph'
            checks.append('unknown-nonreserved-schema-object-is-refused')
        else:
            raise AssertionError('unknown schema object was ignored')
        unknown.close()
        lifecycle = connect(root / 'lifecycle.sqlite'); initialize(lifecycle)
        lifecycle.execute('BEGIN IMMEDIATE'); insert(lifecycle); lifecycle.commit()
        lifecycle.execute("UPDATE capacity_allocations SET phase='held'")
        resumed = payload(); resumed['action'] = 'resume'; resumed['operation'] = (b'r' * 16).hex()
        resumed['launch'] = (b'l' * 16).hex(); resumed['allocations'] = [resumed['allocations'][1]]
        resumed['allocations'][0]['id'] = (b'a' * 16).hex()
        lifecycle.execute('BEGIN IMMEDIATE'); insert(lifecycle, resumed); lifecycle.commit()
        assert inventory(lifecycle, expected)[FS] == (5 * GIB, 80)
        checks.append('resume-adds-growth-without-retained-duplicate')
        for allocation in (GROWTH, b'a' * 16):
            lifecycle.execute("UPDATE capacity_allocations SET phase='release-pending' WHERE allocation_uuid=?", (allocation,))
            lifecycle.execute("UPDATE capacity_allocations SET phase='released',release_sha=? WHERE allocation_uuid=?", (b'p' * 32, allocation))
        lifecycle.execute("UPDATE capacity_allocations SET phase='release-pending' WHERE allocation_uuid=?", (RETAINED,))
        assert inventory(lifecycle, expected)[FS] == (GIB, 16)
        checks.append('retained-release-pending-keeps-full-charge-after-launches')
        lifecycle.execute("UPDATE capacity_allocations SET phase='released',release_sha=? WHERE allocation_uuid=?", (b'p' * 32, RETAINED))
        assert inventory(lifecycle, expected) == {}
        checks.append('retired-history-keeps-valid-resume-tombstone')
        lifecycle.close()
        child = """import os,runpy,sys
m=runpy.run_path(sys.argv[1]); db=m['connect'](sys.argv[2]); db.execute('BEGIN IMMEDIATE'); m['insert'](db)
if sys.argv[3]=='after': db.commit()
os.write(1,b'original-fixture-insert-executed')
os._exit(0)
"""
        for stage in ('before', 'after'):
            crash_path = root / (stage + '.sqlite')
            crash = connect(crash_path); initialize(crash); crash.close()
            result = subprocess.run([sys.executable, '-I', '-c', child, str(Path(__file__).resolve()),
                                     str(crash_path), stage], check=True, stdout=subprocess.PIPE)
            assert result.stdout == b'original-fixture-insert-executed'
            checks.append(stage + '-crash-child-execution')
            crash = connect(crash_path)
            result = inventory(crash, expected)
            assert result == ({} if stage == 'before' else {FS: (3 * GIB, 48)})
            crash.close()
            checks.append(stage + '-commit-crash')
    print(json.dumps({'sqlite_version': sqlite3.sqlite_version, 'checks': checks,
                      'passed': len(checks), 'production_api_exercised': False,
                      'original_release_proof_exercised': False}, sort_keys=True))


if __name__ == '__main__':
    main()
