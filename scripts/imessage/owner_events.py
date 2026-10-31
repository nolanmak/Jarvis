"""Read-only native owner events. Separate from the prose archive exporter.

Capture never dispatches commands. The Linux store authenticates the binding,
deduplicates events, and quarantines self-chat until sent GUID reconciliation.
"""
import re
import sqlite3
from pathlib import Path

_UUID = r"[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}"
_TARGET = re.compile(r"(?:p:[0-9]+/)?(" + _UUID + r")\Z")
_REQUIRED = {"guid", "text", "attributedBody", "is_from_me", "date",
             "associated_message_type", "associated_message_guid",
             "reply_to_guid", "thread_originator_guid", "date_edited", "date_retracted"}


def target_guid(value):
    """Only native plain/part-prefixed UUID forms we can bind exactly."""
    match = _TARGET.fullmatch(value or "")
    return match.group(1).upper() if match else None


def capture(db_path, binding, decode_body=None, limit=100):
    if not 1 <= limit <= 100:
        raise ValueError("owner batch limit must be 1..100")
    if not binding.get("enabled"):
        return {"revision": binding["revision"], "watermark": binding["cursor"], "events": []}
    if "activated_at_ms" not in binding:
        raise RuntimeError("owner binding has no enrollment timestamp")
    con = sqlite3.connect(Path(db_path).resolve().as_uri() + "?mode=ro", uri=True)
    try:
        con.execute("BEGIN")
        columns = {row[1] for row in con.execute("PRAGMA table_info(message)")}
        if not _REQUIRED.issubset(columns):
            raise RuntimeError("native owner capture schema unavailable: " + ",".join(sorted(_REQUIRED - columns)))
        high = con.execute("SELECT COALESCE(MAX(ROWID),0) FROM message").fetchone()[0]
        cursor = binding["cursor"]
        if cursor < 0 or cursor > high:
            raise RuntimeError("native owner cursor invalid; explicit reenrollment required")
        rows = con.execute("""SELECT m.ROWID,m.guid,m.text,m.attributedBody,m.is_from_me,m.date,
            m.associated_message_type,m.associated_message_guid,m.reply_to_guid,
            m.thread_originator_guid,m.date_edited,m.date_retracted,h.id,c.service_name
            FROM message m JOIN chat_message_join j ON j.message_id=m.ROWID
            JOIN chat c ON c.ROWID=j.chat_id LEFT JOIN handle h ON h.ROWID=m.handle_id
            WHERE c.guid=? AND m.ROWID>? AND m.ROWID<=?
            ORDER BY m.ROWID LIMIT ?""", (binding["chat_guid"], cursor, high, limit)).fetchall()
        events = []
        for (rowid, guid, text, blob, from_me, native_date, assoc, associated, reply, origin,
             edited, retracted, sender, service) in rows:
            if service != "iMessage" or bool(from_me) != binding["self_chat"]:
                continue
            if not from_me and sender != binding["owner_handle"]:
                continue
            if not native_date:
                raise RuntimeError("native owner event has no creation date")
            created_ms = (native_date // 1_000_000 if abs(native_date) > 1e12
                          else int(native_date * 1000)) + 978_307_200_000
            if created_ms < binding["activated_at_ms"]:
                continue  # late-synced history is never a new owner instruction
            kind = {2001: "like_add", 3001: "like_remove", 0: "text"}.get(assoc, "other")
            target = target_guid(associated if assoc else (reply or origin))
            # Edited/retracted text must never become an executable replacement.
            if edited or retracted:
                kind = "other"
            body = text or (decode_body(blob) if blob and decode_body else "")
            if len(body) > 8000:
                # Keep a receipt for the event, but never dispatch a truncated command.
                kind, body = "other", "[Owner input exceeds 8000 characters]"
            if not guid:
                raise RuntimeError("native owner event has no GUID")
            events.append({"binding_revision": binding["revision"], "rowid": rowid,
                           "created_at_ms": created_ms,
                           "guid": target_guid(guid) or guid, "chat_guid": binding["chat_guid"],
                           "sender": binding["owner_handle"], "from_me": bool(from_me),
                           "kind": kind, "target_guid": target, "body": body})
        watermark = rows[-1][0] if len(rows) == limit else high
        return {"revision": binding["revision"], "watermark": watermark, "events": events}
    finally:
        con.close()
