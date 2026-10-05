"""Send approved iMessage replies from the Mac and prove the outcome (#1305).

The agent queues approved replies in its outbox. This sender, running on the
Mac signed in to Messages, claims one item at a time through the agent's CLI
(locally or over SSH), sends it with AppleScript, then reads chat.db to decide
what actually happened. osascript's exit code is not evidence: in spike #1280
a send to an undeliverable address exited 0 and only chat.db showed the
failure (error 22, is_sent 0).

At most once: the item is written to a private journal before osascript runs.
A sender that dies mid-send reconciles from chat.db on its next run and never
sends a journalled item again.
"""
import contextlib
import fcntl
import json
import os
from pathlib import Path
import re
import shlex
import sqlite3
import subprocess
import time

from imessage_sync import decode_attributed_body

OSASCRIPT = "/usr/bin/osascript"
# The first Apple event after Messages starts took 60.6 s in #1280.
SEND_TIMEOUT_S = 120
DEFAULT_DEADLINE_S = 60

# Text and target are argv items, never part of the script source. A handle
# addresses a 1:1 chat; a group can only be addressed by the chat guid copied
# from chat.db. Never build a guid from a handle: macOS 26 uses `any;-;`.
SCRIPTS = {
    "handle": """on run argv
tell application "Messages"
set svc to 1st account whose service type = iMessage
send (item 1 of argv) to participant (item 2 of argv) of svc
end tell
end run""",
    "chat_guid": """on run argv
tell application "Messages"
send (item 1 of argv) to chat id (item 2 of argv)
end tell
end run""",
}

HOST_RE = re.compile(r"(?:[A-Za-z0-9_][A-Za-z0-9_.-]*@)?[A-Za-z0-9][A-Za-z0-9_.-]*")
PATH_RE = re.compile(r"/[A-Za-z0-9_./-]*")


class AgentError(RuntimeError):
    pass


class ChatDbUnreadable(RuntimeError):
    pass


class Busy(RuntimeError):
    pass


def _check_path(path, what):
    if not PATH_RE.fullmatch(path) or ".." in path.split("/"):
        raise ValueError(f"{what} must be an absolute path of letters, digits, _ . / -")


class AgentCli:
    """Runs `augmentagent imessage …` on the agent host."""

    def __init__(self, agent_dir, binary="target/release/augmentagent", remote=None,
                 runner=subprocess.run, timeout=60):
        _check_path(agent_dir, "agent directory")
        if not re.fullmatch(r"[A-Za-z0-9_./-]+", binary) or ".." in binary.split("/"):
            raise ValueError("binary must be a plain path")
        if remote is not None and not HOST_RE.fullmatch(remote):
            raise ValueError("remote must be [user@]host")
        self.agent_dir = agent_dir.rstrip("/") or "/"
        self.binary = binary
        self.remote = remote
        self.runner = runner
        self.timeout = timeout

    def command(self, args):
        if self.remote is None:
            binary = self.binary if self.binary.startswith("/") else \
                f"{self.agent_dir}/{self.binary}"
            return [binary, "imessage", *args], self.agent_dir
        binary = self.binary if self.binary.startswith("/") else f"./{self.binary}"
        remote_cmd = f"cd {shlex.quote(self.agent_dir)} && " + " ".join(
            shlex.quote(a) for a in [binary, "imessage", *args])
        control = str(Path.home() / ".ssh" / "aa-imessage-%C")
        return [
            "ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15",
            "-o", "ControlMaster=auto", "-o", f"ControlPath={control}",
            "-o", "ControlPersist=120",
            self.remote, remote_cmd,
        ], None

    def _run(self, args):
        cmd, cwd = self.command(args)
        try:
            done = self.runner(cmd, cwd=cwd, capture_output=True, text=True,
                               timeout=self.timeout)
        except (OSError, subprocess.TimeoutExpired) as e:
            raise AgentError(f"agent command failed: {e}") from e
        if done.returncode != 0:
            raise AgentError(f"agent exited {done.returncode}: {(done.stderr or '').strip()[:300]}")
        return done.stdout

    def claim(self):
        out = self._run(["outbox", "claim", "--json"])
        try:
            data = json.loads(out.strip().splitlines()[-1])
        except (ValueError, IndexError) as e:
            raise AgentError(f"unparseable claim output: {out[:200]!r}") from e
        if data.get("version") != 1:
            raise AgentError(f"unsupported outbox protocol version {data.get('version')!r}")
        return data.get("item")

    def heartbeat(self, error=None):
        args = ["outbox", "heartbeat"]
        if error:
            args += ["--error", error]
        self._run(args)

    def complete(self, item_id, status, error_code=None, reason=None, message_guid=None):
        args = ["outbox", "complete", str(int(item_id)), "--status", status]
        if error_code is not None:
            args += ["--error-code", str(int(error_code))]
        if reason:
            args += ["--reason", reason]
        if message_guid:
            args += ["--message-guid", message_guid]
        self._run(args)


def _outcome(status, error_code=None, reason=None, message_guid=None):
    return {"status": status, "error_code": error_code, "reason": reason,
            "message_guid": message_guid}


class Sender:
    def __init__(self, agent, db, state_dir, runner=subprocess.run, sleep=time.sleep,
                 deadline_s=DEFAULT_DEADLINE_S, poll_s=1, max_items=10,
                 send_timeout_s=SEND_TIMEOUT_S):
        self.agent = agent
        self.db = str(db)
        self.state_dir = Path(state_dir)
        self.runner = runner
        self.sleep = sleep
        self.deadline_s = deadline_s
        self.poll_s = poll_s
        self.max_items = max_items
        self.send_timeout_s = max(send_timeout_s, 90)

    @property
    def journal(self):
        return self.state_dir / "journal.json"

    def _connect(self):
        try:
            con = sqlite3.connect(f"file:{self.db}?mode=ro", uri=True)
            con.execute("SELECT MAX(ROWID) FROM message").fetchone()
            return con
        except sqlite3.Error as e:
            raise ChatDbUnreadable(
                f"cannot read the Messages database ({e}); grant Full Disk Access to "
                "the Python that runs this sender. See docs/IMESSAGE.md.") from e

    def _watermark(self):
        con = self._connect()
        try:
            return con.execute("SELECT COALESCE(MAX(ROWID), 0) FROM message").fetchone()[0]
        finally:
            con.close()

    @contextlib.contextmanager
    def lock(self):
        self.state_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
        with (self.state_dir / ".send.lock").open("a") as fh:
            try:
                fcntl.flock(fh, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as e:
                raise Busy("another sender is running") from e
            yield

    def _write_journal(self, entry):
        tmp = self.journal.with_suffix(".tmp")
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as fh:
            json.dump(entry, fh)
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp, self.journal)

    def run_once(self):
        """Reconcile any interrupted item, then send up to `max_items`.
        Returns how many new items were sent (0 when another sender runs)."""
        try:
            self._connect().close()
        except ChatDbUnreadable:
            # Report a fixed diagnostic, without sending local paths or database text.
            self.agent.heartbeat("Messages database unreadable: grant Full Disk Access to the scheduled Python")
            raise
        self.agent.heartbeat()
        try:
            with self.lock():
                self._reconcile_journal()
                sent = 0
                while sent < self.max_items:
                    item = self.agent.claim()
                    if item is None:
                        break
                    self._process(item)
                    sent += 1
                return sent
        except Busy:
            return 0

    def _reconcile_journal(self):
        if not self.journal.exists():
            return
        entry = json.loads(self.journal.read_text())
        outcome = entry.get("outcome")
        if outcome is None:
            outcome = self._verify(entry, entry["watermark"], deadline_s=0)
            if outcome["status"] == "unknown":
                outcome = _outcome("unknown", outcome["error_code"],
                                   "sender was interrupted and chat.db shows no sent message")
        self._report(entry, outcome)

    def _report(self, entry, outcome):
        entry["outcome"] = outcome
        self._write_journal(entry)
        self.agent.complete(entry["id"], outcome["status"], error_code=outcome["error_code"],
                            reason=outcome["reason"], message_guid=outcome["message_guid"])
        self.journal.unlink()
        print(f"item {entry['id']}: {outcome['status']}", flush=True)

    def _process(self, item):
        kind = item.get("target_kind")
        body = item.get("body") or ""
        entry = {"id": item["id"], "target": item.get("target", ""), "target_kind": kind,
                 "body": body, "watermark": self._watermark(), "state": "dispatching"}
        if kind not in SCRIPTS or not entry["target"] or not body.strip():
            self._report(entry, _outcome("failed", reason="invalid outbox item"))
            return
        # Owner alerts have a short dispatch lease. This bounds the interval
        # between the Linux lifecycle check and invoking Messages, assuming
        # synchronized host clocks. Expired work is never requeued.
        if item.get("send_by_ms") is not None and time.time() * 1000 >= item["send_by_ms"]:
            self._report(entry, _outcome("failed", reason="owner alert expired before dispatch"))
            return
        self._write_journal(entry)
        cmd = [OSASCRIPT, "-e", SCRIPTS[kind], body, entry["target"]]
        try:
            done = self.runner(cmd, capture_output=True, text=True, timeout=self.send_timeout_s)
        except subprocess.TimeoutExpired:
            outcome = self._verify(entry, entry["watermark"], deadline_s=0)
            if outcome["status"] == "unknown":
                outcome = _outcome("unknown", reason=f"osascript timed out after {self.send_timeout_s}s")
            self._report(entry, outcome)
            return
        if done.returncode != 0:
            err = (done.stderr or "").replace(entry["target"], "<target>").strip()[:200]
            outcome = self._verify(entry, entry["watermark"], deadline_s=0)
            if outcome["status"] == "unknown":
                outcome = _outcome("unknown", reason=f"osascript exit {done.returncode}: {err}")
            self._report(entry, outcome)
            return
        self._report(entry, self._verify(entry, entry["watermark"], self.deadline_s))

    def _verify(self, entry, watermark, deadline_s):
        """Wait for our outgoing row: after the watermark, from me, same body,
        in the target chat. The chat join can land after the message row."""
        column = "c.chat_identifier" if entry["target_kind"] == "handle" else "c.guid"
        sql = (
            f"SELECT m.ROWID, m.guid, m.text, m.attributedBody, m.is_sent, m.error, {column} "
            "FROM message m "
            "LEFT JOIN chat_message_join j ON j.message_id = m.ROWID "
            "LEFT JOIN chat c ON c.ROWID = j.chat_id "
            "WHERE m.ROWID > ? AND m.is_from_me = 1 ORDER BY m.ROWID"
        )
        waited = 0
        while True:
            con = self._connect()
            try:
                rows = con.execute(sql, (watermark,)).fetchall()
            finally:
                con.close()
            ours = [r for r in rows if _text(r[2], r[3]) == entry["body"]]
            in_chat = [r for r in ours if r[6] == entry["target"]]
            unjoined = [r for r in ours if r[6] is None]
            if in_chat:
                _, guid, _, _, is_sent, error, _ = in_chat[0]
                if error:
                    return _outcome("failed", error_code=error,
                                    reason=f"Messages reported error {error}")
                if is_sent:
                    return _outcome("sent", message_guid=guid)
            if waited >= deadline_s:
                if in_chat:
                    return _outcome("unknown", reason=f"message not sent within {deadline_s}s")
                if unjoined:
                    return _outcome("unknown", reason="message row never joined the target chat")
                return _outcome("unknown",
                                reason=f"no matching message in chat.db within {deadline_s}s")
            self.sleep(self.poll_s)
            waited += self.poll_s


def _text(text, blob):
    if text is not None:
        return text
    try:
        return decode_attributed_body(blob)
    except Exception:
        return None
