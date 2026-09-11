"""Tests for imessage_sync.

Fixture chat.db is built in-memory with the subset of Apple's schema the
sync reads, so tests run without Full Disk Access or real message data.
"""
import json
import sqlite3
import unittest
import tempfile
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from imessage_sync import (
    apple_time_to_iso,
    build_backfill_plan,
    build_link_tree,
    decode_attributed_body,
    escape_body,
    load_contacts,
    resolve_name,
    slugify,
    sync,
)

APPLE_EPOCH = 978307200  # 2001-01-01 UTC in unix seconds


def make_fixture_db(path):
    con = sqlite3.connect(path)
    cur = con.cursor()
    cur.executescript(
        """
        CREATE TABLE handle (ROWID INTEGER PRIMARY KEY, id TEXT);
        CREATE TABLE chat (
            ROWID INTEGER PRIMARY KEY, guid TEXT, chat_identifier TEXT,
            display_name TEXT, service_name TEXT
        );
        CREATE TABLE message (
            ROWID INTEGER PRIMARY KEY, guid TEXT, text TEXT,
            attributedBody BLOB, date INTEGER, is_from_me INTEGER,
            handle_id INTEGER, cache_has_attachments INTEGER DEFAULT 0,
            associated_message_type INTEGER DEFAULT 0
        );
        CREATE TABLE chat_message_join (chat_id INTEGER, message_id INTEGER);
        CREATE TABLE chat_handle_join (chat_id INTEGER, handle_id INTEGER);
        CREATE TABLE attachment (
            ROWID INTEGER PRIMARY KEY, mime_type TEXT, transfer_name TEXT,
            filename TEXT
        );
        CREATE TABLE message_attachment_join (
            message_id INTEGER, attachment_id INTEGER
        );
        """
    )
    cur.execute("INSERT INTO handle VALUES (1, '+15551234567')")
    cur.execute("INSERT INTO handle VALUES (2, 'friend@example.com')")
    cur.execute(
        "INSERT INTO chat VALUES (1, 'iMessage;-;+15551234567', '+15551234567', NULL, 'iMessage')"
    )
    cur.execute(
        "INSERT INTO chat VALUES (2, 'iMessage;+;chat0001', 'chat0001', 'Ski Trip', 'iMessage')"
    )
    cur.execute("INSERT INTO chat_handle_join VALUES (1, 1)")
    cur.execute("INSERT INTO chat_handle_join VALUES (2, 1)")
    cur.execute("INSERT INTO chat_handle_join VALUES (2, 2)")
    con.commit()
    return con


def add_message(con, rowid, chat_id, text, date_ns, is_from_me, handle_id=1, **kw):
    con.execute(
        "INSERT INTO message (ROWID, guid, text, attributedBody, date, is_from_me,"
        " handle_id, cache_has_attachments, associated_message_type)"
        " VALUES (?,?,?,?,?,?,?,?,?)",
        (
            rowid, f"guid-{rowid}", text, kw.get("attributed_body"),
            date_ns, is_from_me, handle_id,
            kw.get("cache_has_attachments", 0),
            kw.get("associated_message_type", 0),
        ),
    )
    con.execute("INSERT INTO chat_message_join VALUES (?,?)", (chat_id, rowid))
    con.commit()


def ns(unix_seconds):
    return (unix_seconds - APPLE_EPOCH) * 1_000_000_000


def make_contacts_db(path):
    """Minimal replica of the macOS AddressBook (.abcddb) schema."""
    con = sqlite3.connect(path)
    con.executescript(
        """
        CREATE TABLE ZABCDRECORD (
            Z_PK INTEGER PRIMARY KEY, ZFIRSTNAME TEXT, ZLASTNAME TEXT,
            ZORGANIZATION TEXT
        );
        CREATE TABLE ZABCDPHONENUMBER (
            Z_PK INTEGER PRIMARY KEY, ZOWNER INTEGER, ZFULLNUMBER TEXT
        );
        CREATE TABLE ZABCDEMAILADDRESS (
            Z_PK INTEGER PRIMARY KEY, ZOWNER INTEGER, ZADDRESS TEXT
        );
        """
    )
    con.execute("INSERT INTO ZABCDRECORD VALUES (1, 'John', 'Smith', NULL)")
    con.execute(
        "INSERT INTO ZABCDPHONENUMBER VALUES (1, 1, '+1 (555) 123-4567')"
    )
    con.execute("INSERT INTO ZABCDRECORD VALUES (2, NULL, NULL, 'Acme Corp')")
    con.execute(
        "INSERT INTO ZABCDEMAILADDRESS VALUES (1, 2, 'Friend@Example.com')"
    )
    con.commit()
    con.close()
    return path


class TestHelpers(unittest.TestCase):
    def test_apple_time_nanoseconds(self):
        # 2026-08-26 12:00:00 UTC
        iso = apple_time_to_iso(ns(1787745600))
        self.assertTrue(iso.startswith("2026-08-26T"))
        # must carry a UTC offset so the timestamp is unambiguous
        self.assertRegex(iso, r"[+-]\d{2}:\d{2}$")

    def test_apple_time_legacy_seconds(self):
        # pre-High-Sierra databases stored seconds, not nanoseconds
        iso = apple_time_to_iso(1787745600 - APPLE_EPOCH)
        self.assertTrue(iso.startswith("2026-08-26T"))

    def test_apple_time_zero_is_none(self):
        self.assertIsNone(apple_time_to_iso(0))

    def test_decode_attributed_body_single_byte_length(self):
        blob = b"junkNSString\x01\x94\x84\x01+\x05Hellojunk"
        self.assertEqual(decode_attributed_body(blob), "Hello")

    def test_decode_attributed_body_two_byte_length(self):
        text = b"x" * 300
        blob = (
            b"junkNSString\x01\x94\x84\x01+\x81" + (300).to_bytes(2, "little") + text
        )
        self.assertEqual(decode_attributed_body(blob), "x" * 300)

    def test_decode_attributed_body_garbage_is_none(self):
        # documented limitation: undecodable blobs yield None, not garbage
        self.assertIsNone(decode_attributed_body(b"\x00\x01\x02"))
        self.assertIsNone(decode_attributed_body(None))

    def test_escape_body_protects_header_pattern(self):
        body = "normal line\n### [2026-01-01T00:00:00+00:00] fake"
        escaped = escape_body(body)
        self.assertIn("\n\\### [", escaped)
        self.assertEqual(escape_body("plain"), "plain")

    def test_slugify(self):
        self.assertEqual(slugify("+15551234567"), "+15551234567")
        self.assertEqual(slugify("a b/c:d"), "a_b_c_d")
        self.assertEqual(slugify("friend@example.com"), "friend@example.com")


class TestContacts(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.contacts = load_contacts(
            [make_contacts_db(str(Path(self.tmp.name) / "ab.abcddb"))]
        )

    def tearDown(self):
        self.tmp.cleanup()

    def test_phone_resolves_despite_formatting(self):
        # stored as '+1 (555) 123-4567', handle arrives as '+15551234567'
        self.assertEqual(resolve_name(self.contacts, "+15551234567"), "John Smith")

    def test_email_resolves_case_insensitively(self):
        self.assertEqual(
            resolve_name(self.contacts, "friend@example.com"), "Acme Corp"
        )

    def test_unknown_handle_is_none(self):
        self.assertIsNone(resolve_name(self.contacts, "+19998887777"))
        self.assertIsNone(resolve_name(None, "+15551234567"))


class TestSync(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.db_path = str(self.root / "chat.db")
        self.out = self.root / "repo"
        self.state = self.out / ".sync_state.json"
        self.con = make_fixture_db(self.db_path)

    def tearDown(self):
        self.con.close()
        self.tmp.cleanup()

    def read_md(self, ident):
        return (self.out / "conversations" / ident / "messages.md").read_text()

    def test_initial_sync_writes_messages_and_frontmatter(self):
        add_message(self.con, 1, 1, "hey there", ns(1782475200), 0)
        add_message(self.con, 2, 1, "hi!", ns(1782475260), 1)
        result = sync(self.db_path, self.out, self.state)
        self.assertEqual(result["messages"], 2)
        md = self.read_md("+15551234567")
        self.assertTrue(md.startswith("---\n"))
        self.assertIn("type: iMessage Conversation", md)
        self.assertIn("resource: imessage://chat/+15551234567", md)
        self.assertIn("chat_identifier: '+15551234567'", md)
        self.assertIn("- '+15551234567'", md)  # participants
        self.assertIn("] +15551234567\nhey there", md)
        self.assertIn("] me\nhi!", md)
        # chronological order
        self.assertLess(md.index("hey there"), md.index("hi!"))

    def test_incremental_sync_appends_only_new(self):
        add_message(self.con, 1, 1, "first", ns(1782475200), 0)
        sync(self.db_path, self.out, self.state)
        add_message(self.con, 2, 1, "second", ns(1782475300), 1)
        result = sync(self.db_path, self.out, self.state)
        self.assertEqual(result["messages"], 1)
        md = self.read_md("+15551234567")
        self.assertEqual(md.count("first"), 1)
        self.assertEqual(md.count("second"), 1)
        # frontmatter not duplicated on append
        self.assertEqual(md.count("---\n"), 2)

    def test_noop_sync(self):
        add_message(self.con, 1, 1, "only", ns(1782475200), 0)
        sync(self.db_path, self.out, self.state)
        result = sync(self.db_path, self.out, self.state)
        self.assertEqual(result["messages"], 0)

    def test_group_chat_gets_own_dir_and_display_name(self):
        add_message(self.con, 1, 2, "who's driving?", ns(1782475200), 0, handle_id=2)
        sync(self.db_path, self.out, self.state)
        md = self.read_md("chat0001")
        self.assertIn("title: 'Ski Trip'", md)
        self.assertIn("] friend@example.com", md)

    def test_attributed_body_fallback_when_text_null(self):
        blob = b"streamtypedNSString\x01\x94\x84\x01+\x09from blob"
        add_message(self.con, 1, 1, None, ns(1782475200), 0, attributed_body=blob)
        sync(self.db_path, self.out, self.state)
        self.assertIn("from blob", self.read_md("+15551234567"))

    def test_attachment_noted(self):
        self.con.execute(
            "INSERT INTO attachment VALUES (1, 'image/jpeg', 'IMG_001.jpeg', NULL)"
        )
        add_message(
            self.con, 1, 1, None, ns(1782475200), 1, cache_has_attachments=1
        )
        self.con.execute("INSERT INTO message_attachment_join VALUES (1, 1)")
        self.con.commit()
        sync(self.db_path, self.out, self.state)
        self.assertIn("[attachment: image/jpeg IMG_001.jpeg]", self.read_md("+15551234567"))

    def add_attachment(self, rowid, msg_rowid, local_path, name="IMG_001.jpeg"):
        self.con.execute(
            "INSERT INTO attachment VALUES (?, 'image/jpeg', ?, ?)",
            (rowid, name, local_path),
        )
        self.con.execute(
            "INSERT INTO message_attachment_join VALUES (?, ?)",
            (msg_rowid, rowid),
        )
        self.con.commit()

    def test_attachment_uploaded_to_s3_and_referenced(self):
        local = self.root / "IMG_001.jpeg"
        local.write_bytes(b"jpegdata")
        add_message(self.con, 1, 1, None, ns(1782475200), 1,
                    cache_has_attachments=1)
        self.add_attachment(1, 1, str(local))
        uploads = []
        s3 = {"bucket": "test-bucket",
              "uploader": lambda p, b, k: uploads.append((p, b, k)) or True}
        sync(self.db_path, self.out, self.state, s3=s3)
        self.assertEqual(
            uploads,
            [(str(local), "test-bucket", "conversations/+15551234567/attachments/1-IMG_001.jpeg")],
        )
        self.assertIn(
            "[attachment: image/jpeg IMG_001.jpeg"
            " s3://test-bucket/conversations/+15551234567/attachments/1-IMG_001.jpeg]",
            self.read_md("+15551234567"),
        )

    def test_failed_upload_retried_next_sync(self):
        local = self.root / "IMG_001.jpeg"
        local.write_bytes(b"jpegdata")
        add_message(self.con, 1, 1, None, ns(1782475200), 1,
                    cache_has_attachments=1)
        self.add_attachment(1, 1, str(local))
        s3 = {"bucket": "test-bucket", "uploader": lambda p, b, k: False}
        sync(self.db_path, self.out, self.state, s3=s3)
        state = json.loads(self.state.read_text())
        self.assertEqual(len(state["pending_uploads"]), 1)
        # reference is written anyway — the key is deterministic
        self.assertIn("s3://test-bucket/conversations/+15551234567/attachments/1-", self.read_md("+15551234567"))
        ok = []
        s3["uploader"] = lambda p, b, k: ok.append(k) or True
        sync(self.db_path, self.out, self.state, s3=s3)
        self.assertEqual(ok, ["conversations/+15551234567/attachments/1-IMG_001.jpeg"])
        state = json.loads(self.state.read_text())
        self.assertEqual(state["pending_uploads"], [])

    def test_missing_attachment_file_noted_without_s3(self):
        # documented limitation: locally-deleted attachments can't be uploaded
        add_message(self.con, 1, 1, None, ns(1782475200), 1,
                    cache_has_attachments=1)
        self.add_attachment(1, 1, str(self.root / "gone.jpeg"), name="gone.jpeg")
        s3 = {"bucket": "test-bucket", "uploader": lambda p, b, k: True}
        sync(self.db_path, self.out, self.state, s3=s3)
        md = self.read_md("+15551234567")
        self.assertIn("[attachment: image/jpeg gone.jpeg]", md)
        self.assertNotIn("s3://", md)
        state = json.loads(self.state.read_text())
        self.assertEqual(state.get("pending_uploads", []), [])

    def test_backfill_plan_maps_attachments_to_conversation_keys(self):
        local = self.root / "IMG_001.jpeg"
        local.write_bytes(b"x")
        add_message(self.con, 1, 1, "hi", ns(1782475200), 0,
                    cache_has_attachments=1)
        self.add_attachment(1, 1, str(local))
        sync(self.db_path, self.out, self.state)  # builds the index
        index = json.loads((self.out / "conversations" / "index.json").read_text())
        plan = build_backfill_plan(self.db_path, index)
        self.assertEqual(
            plan,
            [(str(local),
              "conversations/+15551234567/attachments/1-IMG_001.jpeg")],
        )

    def test_build_link_tree_stages_plan_as_symlinks(self):
        local = self.root / "IMG_001.jpeg"
        local.write_bytes(b"x")
        staging = self.root / "staging"
        plan = [
            (str(local),
             "conversations/+15551234567/attachments/1-IMG_001.jpeg"),
            (str(self.root / "gone.jpeg"),
             "conversations/+15551234567/attachments/2-gone.jpeg"),
        ]
        staged = build_link_tree(plan, staging)
        self.assertEqual(staged, 1)  # missing local files are skipped
        link = staging / "+15551234567" / "attachments" / "1-IMG_001.jpeg"
        self.assertTrue(link.is_symlink())
        self.assertEqual(link.resolve(), local.resolve())
        # re-staging is idempotent
        self.assertEqual(build_link_tree(plan, staging), 1)

    def test_tapbacks_skipped(self):
        # documented limitation: reactions (Loved/Liked/etc.) are not synced
        add_message(
            self.con, 1, 1, "Loved “hey”", ns(1782475200), 1,
            associated_message_type=2000,
        )
        result = sync(self.db_path, self.out, self.state)
        self.assertEqual(result["messages"], 0)

    def test_empty_message_skipped(self):
        # no text, no decodable body, no attachment -> nothing to write
        add_message(self.con, 1, 1, None, ns(1782475200), 0)
        result = sync(self.db_path, self.out, self.state)
        self.assertEqual(result["messages"], 0)
        # but state still advances so we don't rescan it forever
        state = json.loads(self.state.read_text())
        self.assertEqual(state["last_rowid"], 1)

    def test_index_json_written(self):
        add_message(self.con, 1, 2, "yo", ns(1782475200), 0, handle_id=2)
        sync(self.db_path, self.out, self.state)
        index = json.loads((self.out / "conversations" / "index.json").read_text())
        self.assertEqual(index["chat0001"]["title"], "Ski Trip")
        self.assertIn("friend@example.com", index["chat0001"]["participants"])

    def contacts(self):
        return load_contacts(
            [make_contacts_db(str(self.root / "ab.abcddb"))]
        )

    def test_contact_name_becomes_directory(self):
        add_message(self.con, 1, 1, "hey", ns(1782475200), 0)
        sync(self.db_path, self.out, self.state, contacts=self.contacts())
        md = self.read_md("John_Smith")
        self.assertIn("title: 'John Smith'", md)
        self.assertIn("chat_identifier: '+15551234567'", md)
        self.assertFalse((self.out / "conversations" / "+15551234567").exists())

    def test_existing_directory_migrates_to_contact_name(self):
        add_message(self.con, 1, 1, "first", ns(1782475200), 0)
        sync(self.db_path, self.out, self.state)  # no contacts: raw handle dir
        self.assertTrue((self.out / "conversations" / "+15551234567").exists())
        add_message(self.con, 2, 1, "second", ns(1782475300), 1)
        sync(self.db_path, self.out, self.state, contacts=self.contacts())
        md = self.read_md("John_Smith")
        self.assertIn("first", md)
        self.assertIn("second", md)
        self.assertFalse((self.out / "conversations" / "+15551234567").exists())
        index = json.loads((self.out / "conversations" / "index.json").read_text())
        self.assertEqual(index["+15551234567"]["dir"], "John_Smith")

    def test_unnamed_group_titled_by_member_names(self):
        self.con.execute(
            "INSERT INTO chat VALUES (3, 'iMessage;+;chat0002', 'chat0002', NULL, 'iMessage')"
        )
        self.con.execute("INSERT INTO chat_handle_join VALUES (3, 1)")
        self.con.execute("INSERT INTO chat_handle_join VALUES (3, 2)")
        add_message(self.con, 1, 3, "yo", ns(1782475200), 0, handle_id=2)
        sync(self.db_path, self.out, self.state, contacts=self.contacts())
        md = self.read_md("chat0002")  # group dirs keep their stable id
        self.assertIn("title: 'Acme Corp, John Smith'", md)

    def test_symbol_only_contact_name_keeps_identifier_dir(self):
        # a contact saved as just an emoji slugifies to underscores;
        # the directory must stay the stable handle, the title keeps the emoji
        db = str(self.root / "ab2.abcddb")
        con = sqlite3.connect(db)
        con.executescript(
            "CREATE TABLE ZABCDRECORD (Z_PK INTEGER PRIMARY KEY,"
            " ZFIRSTNAME TEXT, ZLASTNAME TEXT, ZORGANIZATION TEXT);"
            "CREATE TABLE ZABCDPHONENUMBER (Z_PK INTEGER PRIMARY KEY,"
            " ZOWNER INTEGER, ZFULLNUMBER TEXT);"
            "CREATE TABLE ZABCDEMAILADDRESS (Z_PK INTEGER PRIMARY KEY,"
            " ZOWNER INTEGER, ZADDRESS TEXT);"
        )
        con.execute("INSERT INTO ZABCDRECORD VALUES (1, '📫', NULL, NULL)")
        con.execute(
            "INSERT INTO ZABCDPHONENUMBER VALUES (1, 1, '+15551234567')"
        )
        con.commit()
        con.close()
        add_message(self.con, 1, 1, "hi", ns(1782475200), 0)
        sync(self.db_path, self.out, self.state, contacts=load_contacts([db]))
        md = self.read_md("+15551234567")
        self.assertIn("title: '📫'", md)

    def test_okf_index_md_written(self):
        add_message(self.con, 1, 2, "yo", ns(1782475200), 0, handle_id=2)
        sync(self.db_path, self.out, self.state)
        root = (self.out / "index.md").read_text()
        self.assertIn("okf_version: '0.2'", root)
        conv = (self.out / "conversations" / "index.md").read_text()
        self.assertIn("Ski Trip", conv)
        self.assertIn("chat0001/messages.md", conv)


if __name__ == "__main__":
    unittest.main()
