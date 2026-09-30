"""Tests for the Mac-side iMessage sender (#1305).

chat.db is synthetic, osascript is a fake runner and the agent is a fake, so
the suite runs anywhere without Messages, Full Disk Access or a real send.
"""
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import imessage_send as snd

PHONE = "+15555550100"  # pii-ok synthetic
OTHER = "+15555550199"  # pii-ok synthetic
GROUP_GUID = "any;+;chat900"


def make_db(path):
    con = sqlite3.connect(path)
    con.executescript(
        """
        CREATE TABLE chat (ROWID INTEGER PRIMARY KEY, guid TEXT, chat_identifier TEXT);
        CREATE TABLE message (
            ROWID INTEGER PRIMARY KEY, guid TEXT, text TEXT, attributedBody BLOB,
            is_from_me INTEGER, is_sent INTEGER, is_delivered INTEGER, error INTEGER
        );
        CREATE TABLE chat_message_join (chat_id INTEGER, message_id INTEGER);
        """
    )
    con.execute("INSERT INTO chat VALUES (1, 'any;-;+15555550100', '+15555550100')")  # pii-ok
    con.execute("INSERT INTO chat VALUES (2, 'any;-;+15555550199', '+15555550199')")  # pii-ok
    con.execute("INSERT INTO chat VALUES (3, ?, 'chat900')", (GROUP_GUID,))
    con.execute("INSERT INTO message VALUES (1, 'old', 'older', NULL, 1, 1, 1, 0)")
    con.execute("INSERT INTO chat_message_join VALUES (1, 1)")
    con.commit()
    return con


def insert(con, rowid, chat_id, text, from_me=1, sent=1, error=0, join=True, blob=None):
    con.execute(
        "INSERT INTO message VALUES (?, ?, ?, ?, ?, ?, 0, ?)",
        (rowid, f"guid-{rowid}", text, blob, from_me, sent, error),
    )
    if join:
        con.execute("INSERT INTO chat_message_join VALUES (?, ?)", (chat_id, rowid))
    con.commit()


class FakeAgent:
    def __init__(self, items=()):
        self.items = list(items)
        self.completed = []
        self.claims = 0

    def claim(self):
        self.claims += 1
        return self.items.pop(0) if self.items else None

    def complete(self, item_id, status, error_code=None, reason=None, message_guid=None):
        self.completed.append(
            {"id": item_id, "status": status, "error_code": error_code,
             "reason": reason, "message_guid": message_guid}
        )


class FakeRunner:
    """Stands in for subprocess.run(osascript…); `effect` mutates chat.db."""

    def __init__(self, returncode=0, stderr="", effect=None, raise_timeout=False):
        self.calls = []
        self.returncode = returncode
        self.stderr = stderr
        self.effect = effect
        self.raise_timeout = raise_timeout

    def __call__(self, cmd, **kw):
        self.calls.append((cmd, kw))
        if self.raise_timeout:
            raise subprocess.TimeoutExpired(cmd, kw.get("timeout"))
        if self.effect:
            self.effect()
        return subprocess.CompletedProcess(cmd, self.returncode, "", self.stderr)


def item(body="see you at 8", target=PHONE, kind="handle", item_id=5):
    return {"id": item_id, "target": target, "target_kind": kind,
            "service": "iMessage", "body": body}


class SenderTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.db = self.root / "chat.db"
        self.con = make_db(self.db)
        self.addCleanup(self.con.close)
        self.state = self.root / "state"

    def sender(self, agent, runner, **kw):
        kw.setdefault("deadline_s", 3)
        return snd.Sender(agent, str(self.db), self.state, runner=runner,
                          sleep=lambda _: None, **kw)

    def test_failed_delivery_is_reported_with_db_error_code(self):
        agent = FakeAgent([item()])
        runner = FakeRunner(effect=lambda: insert(self.con, 2, 1, "see you at 8", sent=0, error=22))
        self.assertEqual(self.sender(agent, runner).run_once(), 1)
        self.assertEqual(agent.completed[0]["status"], "failed")
        self.assertEqual(agent.completed[0]["error_code"], 22)

    def test_sent_row_in_matching_chat_is_reported_sent_with_guid(self):
        agent = FakeAgent([item()])
        runner = FakeRunner(effect=lambda: insert(self.con, 2, 1, "see you at 8"))
        self.sender(agent, runner).run_once()
        self.assertEqual(agent.completed, [
            {"id": 5, "status": "sent", "error_code": None, "reason": None,
             "message_guid": "guid-2"}])

    def test_chat_join_arriving_late_is_still_sent(self):
        agent = FakeAgent([item()])
        runner = FakeRunner(effect=lambda: insert(self.con, 2, 1, "see you at 8", join=False))
        polls = {"n": 0}

        def sleep(_):
            polls["n"] += 1
            if polls["n"] == 2:
                self.con.execute("INSERT INTO chat_message_join VALUES (1, 2)")
                self.con.commit()

        s = snd.Sender(agent, str(self.db), self.state, runner=runner, sleep=sleep,
                       deadline_s=10)
        s.run_once()
        self.assertEqual(agent.completed[0]["status"], "sent")

    def test_incoming_copy_is_ignored(self):
        def effect():
            insert(self.con, 2, 1, "see you at 8", from_me=0)
            insert(self.con, 3, 1, "see you at 8")

        agent = FakeAgent([item()])
        self.sender(agent, FakeRunner(effect=effect)).run_once()
        self.assertEqual(agent.completed[0]["status"], "sent")
        self.assertEqual(agent.completed[0]["message_guid"], "guid-3")

    def test_row_in_another_chat_does_not_count(self):
        agent = FakeAgent([item()])
        runner = FakeRunner(effect=lambda: insert(self.con, 2, 2, "see you at 8"))
        self.sender(agent, runner).run_once()
        self.assertEqual(agent.completed[0]["status"], "failed")

    def test_no_row_before_deadline_fails_once_without_retry(self):
        agent = FakeAgent([item()])
        runner = FakeRunner()
        self.sender(agent, runner).run_once()
        self.assertEqual(len(runner.calls), 1)
        self.assertEqual(agent.completed[0]["status"], "failed")
        self.assertIn("no matching message", agent.completed[0]["reason"])

    def test_unsent_row_at_deadline_fails(self):
        agent = FakeAgent([item()])
        runner = FakeRunner(effect=lambda: insert(self.con, 2, 1, "see you at 8", sent=0))
        self.sender(agent, runner).run_once()
        self.assertEqual(agent.completed[0]["status"], "failed")
        self.assertIn("not sent", agent.completed[0]["reason"])

    def test_osascript_failure_is_reported_with_applescript_error(self):
        agent = FakeAgent([item()])
        runner = FakeRunner(returncode=1, stderr="execution error: nope (-1728)")
        self.sender(agent, runner).run_once()
        self.assertEqual(agent.completed[0]["status"], "failed")
        self.assertIn("-1728", agent.completed[0]["reason"])

    def test_interrupted_dispatch_is_reconciled_from_chat_db_without_resending(self):
        self.state.mkdir()
        journal = {"id": 5, "target": PHONE, "target_kind": "handle",
                   "body": "see you at 8", "watermark": 1, "state": "dispatching"}
        (self.state / "journal.json").write_text(json.dumps(journal))
        insert(self.con, 2, 1, "see you at 8")
        agent = FakeAgent()
        runner = FakeRunner()
        self.sender(agent, runner).run_once()
        self.assertEqual(runner.calls, [])
        self.assertEqual(agent.completed[0]["status"], "sent")
        self.assertFalse((self.state / "journal.json").exists())

    def test_interrupted_dispatch_with_no_row_is_reported_failed_not_resent(self):
        self.state.mkdir()
        journal = {"id": 5, "target": PHONE, "target_kind": "handle",
                   "body": "see you at 8", "watermark": 1, "state": "dispatching"}
        (self.state / "journal.json").write_text(json.dumps(journal))
        agent = FakeAgent([item(item_id=6)])
        runner = FakeRunner(effect=lambda: insert(self.con, 2, 1, "see you at 8"))
        self.sender(agent, runner).run_once()
        self.assertEqual(agent.completed[0]["id"], 5)
        self.assertEqual(agent.completed[0]["status"], "failed")
        self.assertEqual(len(runner.calls), 1)  # only item 6 was sent

    def test_report_failure_keeps_journal_and_reports_next_run(self):
        agent = FakeAgent([item()])

        def boom(*a, **k):
            raise snd.AgentError("ssh down")

        agent.complete = boom
        runner = FakeRunner(effect=lambda: insert(self.con, 2, 1, "see you at 8"))
        with self.assertRaises(snd.AgentError):
            self.sender(agent, runner).run_once()
        self.assertTrue((self.state / "journal.json").exists())
        agent2 = FakeAgent()
        runner2 = FakeRunner()
        self.sender(agent2, runner2).run_once()
        self.assertEqual(runner2.calls, [])
        self.assertEqual(agent2.completed[0]["status"], "sent")

    def test_unreadable_chat_db_stops_before_claiming(self):
        agent = FakeAgent([item()])
        runner = FakeRunner()
        s = snd.Sender(agent, str(self.root / "missing.db"), self.state, runner=runner,
                       sleep=lambda _: None)
        with self.assertRaises(snd.ChatDbUnreadable) as ctx:
            s.run_once()
        self.assertIn("Full Disk Access", str(ctx.exception))
        self.assertEqual(agent.claims, 0)
        self.assertEqual(runner.calls, [])

    def test_special_characters_reach_osascript_unchanged(self):
        body = 'q" b\\ nl\n e\U0001F600'
        agent = FakeAgent([item(body=body)])
        runner = FakeRunner(effect=lambda: insert(self.con, 2, 1, body))
        self.sender(agent, runner).run_once()
        cmd = runner.calls[0][0]
        self.assertEqual(cmd[0], "/usr/bin/osascript")
        self.assertEqual(cmd[-2:], [body, PHONE])
        self.assertNotIn(body, cmd[2])
        self.assertEqual(agent.completed[0]["status"], "sent")

    def test_attributed_body_only_rows_match(self):
        body = "see you at 8"
        encoded = body.encode()
        blob = b"\x04\x0bstreamtyped\x81\xe8\x03\x84\x01@\x84\x84\x84\x12NSAttributedString\x00" \
               b"\x84\x84\x08NSObject\x00\x85\x92\x84\x84\x84\x08NSString\x01\x94\x84\x01+" \
               + bytes([len(encoded)]) + encoded + b"\x86"
        agent = FakeAgent([item(body=body)])
        runner = FakeRunner(effect=lambda: insert(self.con, 2, 1, None, blob=blob))
        self.sender(agent, runner).run_once()
        self.assertEqual(agent.completed[0]["status"], "sent")

    def test_timeout_is_at_least_90s_and_a_timeout_fails_without_retry(self):
        agent = FakeAgent([item()])
        runner = FakeRunner(raise_timeout=True)
        self.sender(agent, runner).run_once()
        self.assertGreaterEqual(runner.calls[0][1]["timeout"], 90)
        self.assertEqual(len(runner.calls), 1)
        self.assertEqual(agent.completed[0]["status"], "failed")
        self.assertIn("timed out", agent.completed[0]["reason"])

    def test_group_target_uses_chat_id_form_and_guid_match(self):
        agent = FakeAgent([item(target=GROUP_GUID, kind="chat_guid")])
        runner = FakeRunner(effect=lambda: insert(self.con, 2, 3, "see you at 8"))
        self.sender(agent, runner).run_once()
        cmd = runner.calls[0][0]
        self.assertIn("chat id", cmd[2])
        self.assertEqual(cmd[-1], GROUP_GUID)
        self.assertEqual(agent.completed[0]["status"], "sent")

    def test_processes_items_until_queue_is_empty_up_to_limit(self):
        agent = FakeAgent([item(item_id=i, body=f"m{i}") for i in range(1, 5)])
        rowid = {"n": 10}

        def effect():
            rowid["n"] += 1
            insert(self.con, rowid["n"], 1, f"m{rowid['n'] - 10}")

        n = self.sender(agent, FakeRunner(effect=effect), max_items=3).run_once()
        self.assertEqual(n, 3)
        self.assertEqual([c["status"] for c in agent.completed], ["sent"] * 3)

    def test_no_legacy_imessage_guid_prefix_anywhere(self):
        for f in Path(snd.__file__).parent.glob("*.py"):
            self.assertNotIn("iMessage;-;", f.read_text(), f.name)

    def test_second_sender_is_skipped_while_one_runs(self):
        s1 = self.sender(FakeAgent(), FakeRunner())
        with s1.lock():
            s2 = self.sender(FakeAgent([item()]), FakeRunner())
            self.assertEqual(s2.run_once(), 0)


class AgentCommandTests(unittest.TestCase):
    def test_local_agent_runs_cli_in_agent_dir(self):
        a = snd.AgentCli(agent_dir="/opt/jarvis", binary="target/release/augmentagent")
        cmd, cwd = a.command(["outbox", "claim", "--json"])
        self.assertEqual(cmd, ["/opt/jarvis/target/release/augmentagent", "imessage",
                               "outbox", "claim", "--json"])
        self.assertEqual(cwd, "/opt/jarvis")

    def test_remote_agent_uses_ssh_batch_mode_and_quotes_arguments(self):
        a = snd.AgentCli(agent_dir="/home/agent/Jarvis", remote="agent@agent-host")
        cmd, cwd = a.command(["outbox", "complete", "5", "--reason", "it's; rm -rf"])
        self.assertIsNone(cwd)
        self.assertEqual(cmd[0], "ssh")
        self.assertIn("BatchMode=yes", cmd)
        self.assertEqual(cmd[-2], "agent@agent-host")
        remote = cmd[-1]
        self.assertTrue(remote.startswith("cd /home/agent/Jarvis && "))
        self.assertIn("'it'\"'\"'s; rm -rf'", remote)

    def test_bad_remote_or_dir_is_rejected_before_any_process(self):
        for remote, d in [("agent@host;id", "/x"), ("-oProxyCommand=x", "/x"),
                          ("agent@host", "relative"), ("agent@host", "/a b"),
                          ("agent@host", "/a/../b")]:
            with self.assertRaises(ValueError, msg=(remote, d)):
                snd.AgentCli(agent_dir=d, remote=remote)

    def test_claim_parses_versioned_json(self):
        calls = []

        def run(cmd, **kw):
            calls.append(cmd)
            return subprocess.CompletedProcess(
                cmd, 0, '{"version":1,"item":{"id":3,"target":"x","target_kind":"handle",'
                        '"service":"iMessage","body":"b"}}\n', "")

        a = snd.AgentCli(agent_dir="/j", runner=run)
        self.assertEqual(a.claim()["id"], 3)

    def test_claim_rejects_unknown_version_and_errors(self):
        def v2(cmd, **kw):
            return subprocess.CompletedProcess(cmd, 0, '{"version":2,"item":null}', "")

        def fail(cmd, **kw):
            return subprocess.CompletedProcess(cmd, 1, "", "boom")

        with self.assertRaises(snd.AgentError):
            snd.AgentCli(agent_dir="/j", runner=v2).claim()
        with self.assertRaises(snd.AgentError):
            snd.AgentCli(agent_dir="/j", runner=fail).claim()

    def test_complete_builds_flags(self):
        seen = []

        def run(cmd, **kw):
            seen.append(cmd)
            return subprocess.CompletedProcess(cmd, 0, '{"version":1}', "")

        snd.AgentCli(agent_dir="/j", runner=run).complete(
            7, "failed", error_code=22, reason="r", message_guid=None)
        self.assertEqual(seen[0][-8:], ["complete", "7", "--status", "failed",
                                        "--error-code", "22", "--reason", "r"])


class SendCliTests(unittest.TestCase):
    def main(self, argv, error=None, sent=0):
        import send

        class S:
            def __init__(self, *a, **k):
                pass

            def run_once(self):
                if error:
                    raise error
                return sent

        return send.main(argv, sender_factory=S)

    def test_exit_codes(self):
        base = ["--agent-dir", "/opt/jarvis"]
        self.assertEqual(self.main(base, sent=2), 0)
        self.assertEqual(self.main(base, error=snd.ChatDbUnreadable("x Full Disk Access")), 1)
        self.assertEqual(self.main(base, error=snd.AgentError("down")), 2)

    def test_parser_validates_agent_location(self):
        import send
        with self.assertRaises(SystemExit):
            send.parser().parse_args([])
        with self.assertRaises(SystemExit):
            send.main(["--agent-dir", "/x", "--remote", "a;b"])
        with self.assertRaises(SystemExit):
            send.main(["--agent-dir", "relative/dir"])

    def test_schedule_builds_sender_job_with_seconds_interval(self):
        import schedule
        with tempfile.TemporaryDirectory() as home:
            args = ["--agent-dir", "/opt/jarvis", "--remote", "agent@agent-host"]
            plist = schedule.make_plist(args, 1, home, job="send", interval_s=15)
        self.assertEqual(plist["Label"], "org.augmentagent.imessage-send")
        self.assertTrue(plist["ProgramArguments"][1].endswith("send.py"))
        self.assertEqual(plist["ProgramArguments"][2:], args)
        self.assertEqual(plist["StartInterval"], 15)
        self.assertTrue(plist["StandardOutPath"].endswith("imessage-send.log"))

    def test_schedule_keeps_the_export_job_unchanged(self):
        import schedule
        with tempfile.TemporaryDirectory() as home:
            plist = schedule.make_plist(["--no-contacts"], 30, home)
        self.assertEqual(plist["Label"], "org.augmentagent.imessage-sync")
        self.assertEqual(plist["StartInterval"], 1800)


if __name__ == "__main__":
    unittest.main()
