import contextlib
import io
import json
import sqlite3
import sys
import tempfile
from pathlib import Path
from types import SimpleNamespace

sys.path.insert(0, str(Path.cwd()))
sys.path.insert(0, str(Path.cwd() / "scripts" / "imessage"))
import imessage_sync
import sync as sync_entry


def captured_failures(items, uploader):
    stream = io.StringIO()
    with contextlib.redirect_stderr(stream):
        imessage_sync.drain_uploads(items, "private-bucket", uploader, exists=lambda _: True, workers=1)
    return [line for line in stream.getvalue().splitlines() if line]


def main():
    same_items = [{"path": f"fixture-{n}", "key": f"item-{n}"} for n in range(50)]
    same_lines = captured_failures(same_items, lambda *_: (_ for _ in ()).throw(RuntimeError("same-cause")))
    distinct_lines = captured_failures(same_items, lambda _p, _b, key: (_ for _ in ()).throw(RuntimeError("cause-" + key)))
    success_lines = captured_failures(same_items, lambda *_: True)

    with tempfile.TemporaryDirectory() as raw:
        root = Path(raw)
        db = root / "chat.db"
        con = sqlite3.connect(db)
        con.executescript("""
            CREATE TABLE handle (ROWID INTEGER PRIMARY KEY, id TEXT);
            CREATE TABLE chat (ROWID INTEGER PRIMARY KEY, guid TEXT, chat_identifier TEXT, display_name TEXT, service_name TEXT);
            CREATE TABLE message (ROWID INTEGER PRIMARY KEY, text TEXT, attributedBody BLOB, date INTEGER, is_from_me INTEGER, handle_id INTEGER, cache_has_attachments INTEGER, associated_message_type INTEGER);
            CREATE TABLE chat_message_join (chat_id INTEGER, message_id INTEGER);
            CREATE TABLE chat_handle_join (chat_id INTEGER, handle_id INTEGER);
            CREATE TABLE attachment (ROWID INTEGER PRIMARY KEY, mime_type TEXT, transfer_name TEXT, filename TEXT);
            CREATE TABLE message_attachment_join (message_id INTEGER, attachment_id INTEGER);
        """)
        con.close()
        out = root / "bundle"
        args = SimpleNamespace(db=db, out=out, remote=None, s3_bucket=None, aws_profile=None, no_contacts=True)
        healthy = io.StringIO()
        with contextlib.redirect_stdout(healthy):
            sync_entry.run(args)
        healthy_lines = [line for line in healthy.getvalue().splitlines() if line]

    print(json.dumps([len(same_lines), any("50" in line for line in same_lines), len(distinct_lines), len(success_lines), len(healthy_lines)]))


if __name__ == "__main__":
    main()
