"""Tests for whatsapp_sync.

Fixture ChatStorage.sqlite is built with the subset of WhatsApp Desktop's
Core Data schema the sync reads, so tests run without the real database.
"""
import json
import sqlite3
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from whatsapp_sync import (
    clean_title,
    core_data_time_to_iso,
    escape_body,
    jid_handle,
    slugify,
    sync,
)

APPLE_EPOCH = 978307200  # 2001-01-01 UTC


def make_fixture_db(path):
    con = sqlite3.connect(path)
    con.executescript(
        """
        CREATE TABLE ZWACHATSESSION (
            Z_PK INTEGER PRIMARY KEY, ZCONTACTJID TEXT, ZPARTNERNAME TEXT,
            ZSESSIONTYPE INTEGER DEFAULT 0
        );
        CREATE TABLE ZWAMESSAGE (
            Z_PK INTEGER PRIMARY KEY, ZCHATSESSION INTEGER, ZTEXT TEXT,
            ZMESSAGEDATE REAL, ZISFROMME INTEGER, ZFROMJID TEXT,
            ZGROUPMEMBER INTEGER, ZMESSAGETYPE INTEGER DEFAULT 0,
            ZPUSHNAME TEXT, ZMEDIAITEM INTEGER, ZGROUPEVENTTYPE INTEGER
        );
        CREATE TABLE ZWAGROUPMEMBER (
            Z_PK INTEGER PRIMARY KEY, ZMEMBERJID TEXT, ZCONTACTNAME TEXT
        );
        CREATE TABLE ZWAMEDIAITEM (
            Z_PK INTEGER PRIMARY KEY, ZMEDIALOCALPATH TEXT, ZTITLE TEXT,
            ZFILESIZE INTEGER
        );
        """
    )
    con.execute(
        "INSERT INTO ZWACHATSESSION VALUES (1, '14155550123@s.whatsapp.net',"
        " '‪John Smith‬', 0)"
    )
    con.execute(
        "INSERT INTO ZWACHATSESSION VALUES (2, '12036000000000@g.us', 'Ski Trip', 1)"
    )
    con.execute(
        "INSERT INTO ZWACHATSESSION VALUES (3, 'status@broadcast', '', 2)"
    )
    con.execute(
        "INSERT INTO ZWAGROUPMEMBER VALUES (7, '14155550999@s.whatsapp.net', 'Jane')"
    )
    con.commit()
    return con


def cd(unix_seconds):
    return unix_seconds - APPLE_EPOCH


def add_message(con, pk, session, text, date, from_me, **kw):
    con.execute(
        "INSERT INTO ZWAMESSAGE (Z_PK, ZCHATSESSION, ZTEXT, ZMESSAGEDATE,"
        " ZISFROMME, ZFROMJID, ZGROUPMEMBER, ZMESSAGETYPE, ZPUSHNAME, ZMEDIAITEM)"
        " VALUES (?,?,?,?,?,?,?,?,?,?)",
        (
            pk, session, text, date, from_me, kw.get("from_jid"),
            kw.get("group_member"), kw.get("message_type", 0),
            kw.get("push_name"), kw.get("media_item"),
        ),
    )
    con.commit()


class TestHelpers(unittest.TestCase):
    def test_core_data_time(self):
        iso = core_data_time_to_iso(cd(1787745600))  # 2026-08-26 12:00 UTC
        self.assertTrue(iso.startswith("2026-08-26T"))
        self.assertRegex(iso, r"[+-]\d{2}:\d{2}$")
        self.assertIsNone(core_data_time_to_iso(None))
        self.assertIsNone(core_data_time_to_iso(0))

    def test_jid_handle(self):
        self.assertEqual(jid_handle("14155550123@s.whatsapp.net"), "+14155550123")
        self.assertEqual(jid_handle("12036000000000@g.us"), "12036000000000@g.us")
        self.assertIsNone(jid_handle(None))

    def test_clean_title_strips_bidi_and_nbsp(self):
        self.assertEqual(clean_title("‪John Smith‬"), "John Smith")
        self.assertEqual(clean_title("plain"), "plain")

    def test_escape_body(self):
        out = escape_body("x\n### [2026-01-01T00:00:00+00:00] fake")
        self.assertIn("\n\\### [", out)


class TestSync(unittest.TestCase):
    def test_three_same_named_contacts_have_distinct_directories(self):
        for pk, jid in [(4, "15550000004@s.whatsapp.net"), (5, "15550000005@s.whatsapp.net")]:
            self.con.execute("INSERT INTO ZWACHATSESSION VALUES (?, ?, 'John Smith', 0)", (pk, jid))
        for pk in (1, 4, 5):
            add_message(self.con, pk, pk, f"unique-{pk}", cd(1787745600 + pk), 0)
        sync(self.db, self.out, self.state)
        index = json.loads((self.out / "conversations/index.json").read_text())
        self.assertEqual(len({v['dir'] for v in index.values()}), 3)

    def test_snapshot_reads_committed_wal_without_raw_file_copy(self):
        from unittest.mock import patch
        self.con.execute("PRAGMA journal_mode=WAL")
        add_message(self.con, 1, 1, "still in WAL", cd(1787745600), 0)
        # Raw copying an open DB and its changing WAL can mix snapshots.
        with patch("shutil.copy", side_effect=AssertionError("use SQLite backup API")):
            result = sync(self.db, self.out, self.state)
        self.assertEqual(result["messages"], 1)
        self.assertIn("still in WAL", self.read_md("John_Smith"))

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.db = str(self.root / "ChatStorage.sqlite")
        self.out = self.root / "repo"
        self.state = self.out / ".sync_state.json"
        self.con = make_fixture_db(self.db)

    def tearDown(self):
        self.con.close()
        self.tmp.cleanup()

    def read_md(self, d):
        return (self.out / "conversations" / d / "messages.md").read_text()

    def test_dm_synced_with_contact_name_dir_and_frontmatter(self):
        add_message(self.con, 1, 1, "hey", cd(1787745600), 0)
        add_message(self.con, 2, 1, "yo", cd(1787745660), 1)
        r = sync(self.db, self.out, self.state)
        self.assertEqual(r["messages"], 2)
        md = self.read_md("John_Smith")
        self.assertIn("type: WhatsApp Conversation", md)
        self.assertIn("title: 'John Smith'", md)
        self.assertIn("service: WhatsApp", md)
        self.assertIn("chat_identifier: '14155550123@s.whatsapp.net'", md)
        self.assertIn("- '+14155550123'", md)  # participants as E.164
        self.assertIn("] +14155550123\nhey", md)
        self.assertIn("] me\nyo", md)

    def test_group_sender_resolved_via_member_table(self):
        add_message(self.con, 1, 2, "ski?", cd(1787745600), 0,
                    from_jid="12036000000000@g.us", group_member=7)
        sync(self.db, self.out, self.state)
        md = self.read_md("Ski_Trip")
        self.assertIn("] +14155550999\nski?", md)

    def test_status_broadcast_skipped(self):
        add_message(self.con, 1, 3, "status junk", cd(1787745600), 0)
        r = sync(self.db, self.out, self.state)
        self.assertEqual(r["messages"], 0)

    def test_incremental_appends_only_new(self):
        add_message(self.con, 1, 1, "first", cd(1787745600), 0)
        sync(self.db, self.out, self.state)
        add_message(self.con, 2, 1, "second", cd(1787745700), 1)
        r = sync(self.db, self.out, self.state)
        self.assertEqual(r["messages"], 1)
        md = self.read_md("John_Smith")
        self.assertEqual(md.count("first"), 1)
        self.assertEqual(md.count("second"), 1)

    def test_media_message_noted(self):
        self.con.execute(
            "INSERT INTO ZWAMEDIAITEM VALUES (5, 'Media/x/IMG_1.jpg', NULL, 12345)"
        )
        add_message(self.con, 1, 1, None, cd(1787745600), 1,
                    message_type=1, media_item=5)
        r = sync(self.db, self.out, self.state)
        self.assertEqual(r["messages"], 1)
        self.assertIn("[attachment: whatsapp-media IMG_1.jpg]", self.read_md("John_Smith"))

    def test_empty_system_message_skipped_but_cursor_advances(self):
        add_message(self.con, 1, 1, None, cd(1787745600), 0, message_type=6)
        r = sync(self.db, self.out, self.state)
        self.assertEqual(r["messages"], 0)
        state = json.loads(self.state.read_text())
        self.assertEqual(state["last_pk"], 1)

    def test_formatted_phone_partner_name_uses_e164_dir(self):
        # unsaved contacts have ZPARTNERNAME like '‪+1 (555) 207‑2258‬' —
        # the directory must be the clean handle, not slugified punctuation
        self.con.execute(
            "INSERT INTO ZWACHATSESSION VALUES (4, '15552072258@s.whatsapp.net',"
            " '‪+1 (555) 207‑2258‬', 0)"
        )
        self.con.commit()
        add_message(self.con, 1, 4, "hi", cd(1787745600), 0)
        sync(self.db, self.out, self.state)
        md = self.read_md("+15552072258")
        self.assertIn("title: '+15552072258'", md)

    def test_slug_collapses_runs_and_trims_underscores(self):
        self.assertEqual(slugify("💼Co nnect Ambassadors🚀 "), "Co_nnect_Ambassadors")
        self.assertEqual(slugify("+14155550123"), "+14155550123")

    def test_indexes_written(self):
        add_message(self.con, 1, 1, "hey", cd(1787745600), 0)
        sync(self.db, self.out, self.state)
        root = (self.out / "index.md").read_text()
        self.assertIn("okf_version: '0.2'", root)
        idx = json.loads((self.out / "conversations" / "index.json").read_text())
        self.assertEqual(idx["14155550123@s.whatsapp.net"]["title"], "John Smith")
        self.assertEqual(idx["14155550123@s.whatsapp.net"]["dir"], "John_Smith")
        conv = (self.out / "conversations" / "index.md").read_text()
        self.assertIn("John_Smith/messages.md", conv)


if __name__ == "__main__":
    unittest.main()
