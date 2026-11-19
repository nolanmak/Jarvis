import importlib
import json
import os
import sqlite3
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path.cwd()))
sys.path.insert(0, str(Path.cwd() / 'scripts' / 'imessage'))
module = importlib.import_module('imessage_sync')
build_backfill_plan = module.build_backfill_plan
sync = module.sync
heic_to_jpeg = getattr(module, 'heic_to_jpeg', None)


def make_db(root):
    root.mkdir(parents=True, exist_ok=True)
    db = root / 'chat.db'
    con = sqlite3.connect(db)
    con.executescript('''
CREATE TABLE handle (ROWID INTEGER PRIMARY KEY, id TEXT);
CREATE TABLE chat (ROWID INTEGER PRIMARY KEY, guid TEXT, chat_identifier TEXT, display_name TEXT, service_name TEXT);
CREATE TABLE message (ROWID INTEGER PRIMARY KEY, guid TEXT, text TEXT, attributedBody BLOB, date INTEGER, is_from_me INTEGER, handle_id INTEGER, cache_has_attachments INTEGER, associated_message_type INTEGER);
CREATE TABLE chat_message_join (chat_id INTEGER, message_id INTEGER);
CREATE TABLE chat_handle_join (chat_id INTEGER, handle_id INTEGER);
CREATE TABLE attachment (ROWID INTEGER PRIMARY KEY, mime_type TEXT, transfer_name TEXT, filename TEXT);
CREATE TABLE message_attachment_join (message_id INTEGER, attachment_id INTEGER);
''')
    con.execute("INSERT INTO handle VALUES (1, '+15550001111')")
    con.execute("INSERT INTO chat VALUES (1, 'iMessage;-;+15550001111', '+15550001111', NULL, 'iMessage')")
    con.execute('INSERT INTO chat_handle_join VALUES (1, 1)')
    con.execute("INSERT INTO message VALUES (1, 'm1', NULL, NULL, 1, 1, 1, 1, 0)")
    con.execute('INSERT INTO chat_message_join VALUES (1, 1)')
    con.commit()
    return con, db


def install_sips(root, succeeds):
    binary = root / 'bin' / 'sips'
    binary.parent.mkdir(parents=True, exist_ok=True)
    body = '#!/usr/bin/env python3\nimport pathlib, sys\n'
    if succeeds:
        body += "out = pathlib.Path(sys.argv[sys.argv.index('--out') + 1]); out.parent.mkdir(parents=True, exist_ok=True); out.write_bytes(b'jpeg')\n"
    else:
        body += 'sys.exit(1)\n'
    binary.write_text(body)
    binary.chmod(0o755)
    os.environ['PATH'] = str(binary.parent) + os.pathsep + os.environ.get('PATH', '')


def line(out):
    text = (out / 'conversations' / '+15550001111' / 'messages.md').read_text()
    return next(value for value in text.splitlines() if value.startswith('[attachment:'))


def run_case(root, suffix, mode, upload_ok):
    con, db = make_db(root)
    source = root / ('photo' + suffix)
    source.write_bytes(b'synthetic-heic')
    mime = 'image/heif' if suffix.lower() == '.heif' else 'image/heic'
    con.execute('INSERT INTO attachment VALUES (1, ?, ?, ?)', (mime, source.name, str(source)))
    con.execute('INSERT INTO message_attachment_join VALUES (1, 1)')
    con.commit()
    con.close()
    converted = root / 'converted'
    uploads = []

    def uploader(path, bucket, key, deadline=None):
        uploads.append(path)
        return upload_ok

    s3 = {'bucket': 'bucket', 'uploader': uploader}
    if mode:
        install_sips(root, mode == 'success')
        s3['converter'] = heic_to_jpeg or (lambda src, dest: False)
        s3['convert_dir'] = converted
    out = root / 'out'
    state = out / '.sync_state.json'
    sync(db, out, state, s3=s3)
    pending = json.loads(state.read_text()).get('pending_uploads', [])
    result = {
        'line': line(out),
        'uploaded_suffix': Path(uploads[0]).suffix,
        'queued_suffixes': [Path(item['path']).suffix for item in pending],
    }
    if mode == 'success':
        result['converted_exists_after_upload'] = bool(
            pending and Path(pending[0]['path']).exists()
        )
    index = json.loads((out / 'conversations' / 'index.json').read_text())
    result['backfill_suffixes'] = [Path(path).suffix for path, _ in build_backfill_plan(db, index)]
    return result


with tempfile.TemporaryDirectory() as temporary:
    root = Path(temporary)
    heic = run_case(root / 'heic', '.HEIC', 'success', True)
    heif = run_case(root / 'heif', '.HEIF', 'success', True)
    failed = run_case(root / 'failed', '.HEIC', 'fail', True)
    unavailable = run_case(root / 'unavailable', '.HEIC', None, True)
    queued = run_case(root / 'queued', '.HEIC', 'success', False)
    print(json.dumps({
        'heic': {key: heic[key] for key in ('line', 'uploaded_suffix', 'queued_suffixes', 'converted_exists_after_upload')},
        'heif': {key: heif[key] for key in ('line', 'uploaded_suffix', 'queued_suffixes', 'converted_exists_after_upload')},
        'failed': {key: failed[key] for key in ('line', 'uploaded_suffix', 'queued_suffixes')},
        'unavailable': {key: unavailable[key] for key in ('line', 'uploaded_suffix', 'queued_suffixes')},
        'queued': {key: queued[key] for key in ('line', 'uploaded_suffix', 'queued_suffixes', 'converted_exists_after_upload')},
        'backfill_suffixes': heic['backfill_suffixes'],
    }))
