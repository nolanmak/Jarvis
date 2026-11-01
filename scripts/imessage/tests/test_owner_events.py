import importlib.util
from pathlib import Path
import sqlite3
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("owner_events", Path(__file__).parents[1] / "owner_events.py")
owner_events = importlib.util.module_from_spec(spec)
spec.loader.exec_module(owner_events)
GUID = "12345678-1234-1234-1234-123456789ABC"


class CaptureTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.db = Path(self.tmp.name) / "chat.db"
        self.con = sqlite3.connect(self.db)
        self.addCleanup(self.con.close)
        self.con.executescript("""
            CREATE TABLE message(guid,text,attributedBody,is_from_me,date,
            associated_message_type DEFAULT 0,associated_message_guid,reply_to_guid,
            thread_originator_guid,date_edited DEFAULT 0,date_retracted DEFAULT 0,handle_id);
            CREATE TABLE chat(guid,service_name);
            CREATE TABLE handle(id);
            CREATE TABLE chat_message_join(message_id,chat_id);
            INSERT INTO chat VALUES('owner-chat','iMessage');
            INSERT INTO handle VALUES('owner@example.invalid');
            INSERT INTO handle VALUES('stranger@example.invalid');
        """)
        self.binding = dict(revision=1, enabled=True, chat_guid="owner-chat",
                            owner_handle="owner@example.invalid", self_chat=False, cursor=0, activated_at_ms=0)

    def add(self, **fields):
        data = dict(guid=GUID, text="approve abcdef01", is_from_me=0, date=1, handle_id=1)
        data.update(fields)
        keys = list(data)
        row = self.con.execute("INSERT INTO message(" + ",".join(keys) + ") VALUES(" +
                               ",".join("?" for _ in keys) + ")", [data[k] for k in keys]).lastrowid
        self.con.execute("INSERT INTO chat_message_join VALUES(?,1)", [row])
        self.con.commit()
        return row

    def capture(self, **kw):
        return owner_events.capture(self.db, self.binding, **kw)

    def test_reply_and_native_reaction_target_are_preserved(self):
        self.add(reply_to_guid=GUID.lower())
        self.add(guid="reaction-guid", associated_message_type=2001,
                 associated_message_guid="p:0/" + GUID)
        self.add(guid="removal-guid", associated_message_type=3001,
                 associated_message_guid="p:1/" + GUID)
        events = self.capture()["events"]
        self.assertEqual([e["kind"] for e in events], ["text", "like_add", "like_remove"])
        self.assertTrue(all(e["target_guid"] == GUID for e in events))

    def test_unbound_sender_direction_and_history_are_excluded(self):
        self.add(handle_id=2)
        self.add(is_from_me=1)
        last = self.add(guid="latest")
        self.binding["cursor"] = last
        self.assertEqual(self.capture()["events"], [])
        self.binding["cursor"] = 0
        self.assertEqual([e["guid"] for e in self.capture()["events"]], ["latest"])

    def test_self_chat_does_not_depend_on_handle_join(self):
        self.binding["self_chat"] = True
        self.add(handle_id=None, is_from_me=1)
        e = self.capture()["events"][0]
        self.assertEqual(e["sender"], self.binding["owner_handle"])
        self.assertTrue(e["from_me"])

    def test_batch_watermark_never_skips_remaining_owner_events(self):
        for i in range(3):
            self.add(guid="event-" + str(i))
        batch = self.capture(limit=2)
        self.assertEqual(batch["watermark"], 2)
        self.binding["cursor"] = batch["watermark"]
        self.assertEqual([e["guid"] for e in self.capture()["events"]], ["event-2"])

    def test_edited_and_oversized_controls_are_not_executable(self):
        self.add(date_edited=1)
        self.add(guid="long", text="x" * 8001)
        self.assertEqual([e["kind"] for e in self.capture()["events"]], ["other", "other"])

    def test_missing_schema_and_cursor_rollback_fail_explicitly(self):
        self.binding["cursor"] = 500
        with self.assertRaisesRegex(RuntimeError, "cursor"):
            self.capture()
        self.binding["cursor"] = 0
        self.con.execute("ALTER TABLE message DROP COLUMN reply_to_guid")
        self.con.commit()
        with self.assertRaisesRegex(RuntimeError, "schema"):
            self.capture()

    def test_unknown_target_is_not_guessed_from_prose(self):
        self.add(associated_message_type=2001, associated_message_guid="Liked a message")
        self.assertIsNone(self.capture()["events"][0]["target_guid"])

    def test_late_synced_history_advances_cursor_without_becoming_a_command(self):
        self.binding["activated_at_ms"] = 1_778_307_202_000
        self.add(date=800_000_001)
        newest = self.add(guid="current", date=800_000_003_000_000_000)
        result = self.capture()
        self.assertEqual(result["watermark"], newest)
        self.assertEqual([e["guid"] for e in result["events"]], ["current"])


if __name__ == "__main__":
    unittest.main()
