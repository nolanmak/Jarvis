"""Sync iMessage conversations from the macOS Messages database into an
OKF v0.2 knowledge bundle: one directory per conversation containing an
append-only `messages.md` with YAML frontmatter.

Reads chat.db strictly read-only. Incremental state (last synced message
ROWID) lives in `.sync_state.json` at the bundle root.
"""
import json
import hashlib
import os
import re
import sqlite3
import tempfile
from datetime import datetime, timezone
from pathlib import Path

VERSION = "0.1"
APPLE_EPOCH = 978307200  # 2001-01-01 00:00:00 UTC in unix seconds

# Tapbacks (2000-2005) and their removals (3000-3005) are not synced.
_NORMAL_ASSOC_TYPES = (0, None)

HEADER_PREFIX = "### ["


def apple_time_to_iso(raw):
    """Apple stores message.date as ns since 2001-01-01 (seconds on
    pre-High-Sierra databases). Returns local ISO-8601 with UTC offset."""
    if not raw:
        return None
    seconds = raw / 1_000_000_000 if abs(raw) > 1e12 else raw
    dt = datetime.fromtimestamp(seconds + APPLE_EPOCH, tz=timezone.utc)
    return dt.astimezone().isoformat(timespec="seconds")


def decode_attributed_body(blob):
    """Best-effort extraction of message text from the typedstream blob
    modern macOS uses when message.text is NULL. Returns None whenever the
    blob doesn't match the known layout — never partial garbage."""
    if not blob:
        return None
    idx = blob.find(b"NSString")
    if idx == -1:
        return None
    idx += len(b"NSString") + 5  # class metadata bytes before the length
    if idx >= len(blob):
        return None
    length = blob[idx]
    idx += 1
    if length == 0x81:  # two-byte little-endian length follows
        if idx + 2 > len(blob):
            return None
        length = int.from_bytes(blob[idx:idx + 2], "little")
        idx += 2
    data = blob[idx:idx + length]
    if not data:
        return None
    return data.decode("utf-8", errors="replace")


def escape_body(text):
    """Message lines that would parse as an entry header get a leading
    backslash, keeping `### [` unambiguous for consumers."""
    return "\n".join(
        "\\" + line if line.startswith(HEADER_PREFIX) else line
        for line in text.split("\n")
    )


def slugify(name):
    slug = re.sub(r"[^A-Za-z0-9+@._-]", "_", name)
    if slug in ("", ".", ".."):
        return "chat-" + hashlib.sha256(name.encode()).hexdigest()[:16]
    return slug


def _atomic_text(path, text):
    """Readers see a complete index/cursor, even if a write is interrupted."""
    path = Path(path)
    fd, temporary = tempfile.mkstemp(dir=path.parent, prefix=".imessage-")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            stream.write(text)
        os.replace(temporary, path)
    finally:
        Path(temporary).unlink(missing_ok=True)


def load_contacts(db_paths):
    """Build {handle_key: display name} from macOS AddressBook databases.
    Phone keys are digit-only (full and last-10); email keys lowercase.
    Unreadable databases are skipped — contacts are best-effort."""
    contacts = {}

    def name_of(first, last, org):
        return " ".join(p for p in (first, last) if p).strip() or org

    for path in db_paths:
        try:
            con = sqlite3.connect(Path(path).resolve().as_uri() + "?mode=ro", uri=True)
            cur = con.cursor()
            for first, last, org, number in cur.execute(
                "SELECT r.ZFIRSTNAME, r.ZLASTNAME, r.ZORGANIZATION,"
                " p.ZFULLNUMBER FROM ZABCDPHONENUMBER p"
                " JOIN ZABCDRECORD r ON r.Z_PK = p.ZOWNER"
            ):
                name = name_of(first, last, org)
                digits = re.sub(r"\D", "", number or "")
                if name and digits:
                    contacts.setdefault(digits, name)
                    if len(digits) >= 10:
                        contacts.setdefault(digits[-10:], name)
            for first, last, org, addr in cur.execute(
                "SELECT r.ZFIRSTNAME, r.ZLASTNAME, r.ZORGANIZATION,"
                " e.ZADDRESS FROM ZABCDEMAILADDRESS e"
                " JOIN ZABCDRECORD r ON r.Z_PK = e.ZOWNER"
            ):
                name = name_of(first, last, org)
                if name and addr:
                    contacts.setdefault(addr.lower(), name)
            con.close()
        except sqlite3.Error:
            continue
    return contacts


def resolve_name(contacts, handle):
    if not contacts or not handle:
        return None
    if "@" in handle:
        return contacts.get(handle.lower())
    digits = re.sub(r"\D", "", handle)
    if digits in contacts:
        return contacts[digits]
    if len(digits) >= 10:
        return contacts.get(digits[-10:])
    return None


def _yq(value):
    """Single-quoted YAML scalar."""
    return "'" + str(value).replace("'", "''") + "'"


def _load_json(path, default):
    path = Path(path)
    if path.exists():
        return json.loads(path.read_text())
    return default


def _participants(cur, chat_rowid):
    rows = cur.execute(
        "SELECT h.id FROM chat_handle_join j"
        " JOIN handle h ON h.ROWID = j.handle_id"
        " WHERE j.chat_id = ? ORDER BY h.id",
        (chat_rowid,),
    ).fetchall()
    return [r[0] for r in rows]


def _attachments(cur, message_rowid):
    return cur.execute(
        "SELECT a.ROWID, a.mime_type, a.transfer_name, a.filename"
        " FROM message_attachment_join j"
        " JOIN attachment a ON a.ROWID = j.attachment_id"
        " WHERE j.message_id = ?",
        (message_rowid,),
    ).fetchall()


def attachment_key(conv_dir, attachment_rowid, name):
    """S3 key mirroring the repo layout: conversations/<dir>/attachments/."""
    return (
        f"conversations/{conv_dir}/attachments/"
        f"{attachment_rowid}-{slugify(name or 'file')}"
    )


def build_backfill_plan(db_path, index):
    """(local_path, s3_key) for every attachment in the database, keyed by
    the conversation directory recorded in index.json."""
    con = sqlite3.connect(Path(db_path).resolve().as_uri() + "?mode=ro", uri=True)
    plan, seen = [], set()
    for att_id, name, filename, identifier in con.execute(
        "SELECT a.ROWID, a.transfer_name, a.filename, c.chat_identifier"
        " FROM attachment a"
        " JOIN message_attachment_join j ON j.attachment_id = a.ROWID"
        " JOIN chat_message_join cmj ON cmj.message_id = j.message_id"
        " JOIN chat c ON c.ROWID = cmj.chat_id"
        " ORDER BY a.ROWID"
    ):
        if att_id in seen or not filename:
            continue
        seen.add(att_id)
        ident = slugify(identifier)
        conv_dir = index.get(ident, {}).get("dir", ident)
        plan.append((
            str(Path(filename).expanduser()),
            attachment_key(conv_dir, att_id, name),
        ))
    con.close()
    return plan


def _naming(ident, display_name, service, participants, contacts, index):
    """Compute title, description, and directory for a conversation.
    Groups keep their stable id as directory; DMs use the contact name
    when one resolves. Collisions get an identifier suffix."""
    is_group = ident.startswith("chat") or len(participants) > 1
    if is_group:
        title = display_name or ", ".join(
            sorted(resolve_name(contacts, p) or p for p in participants)
        ) or ident
        dirname = ident
        desc = f"Group conversation '{title}'"
    else:
        handle = participants[0] if participants else ident
        name = resolve_name(contacts, handle)
        title = name or display_name or ident
        dirname = slugify(title) if name else ident
        if not re.search(r"[A-Za-z0-9]", dirname):
            dirname = ident
        desc = f"Conversation with {title}"
    taken = {e["dir"] for k, e in index.items() if k != ident}
    if dirname in taken:
        dirname = f"{dirname}-{slugify(ident)[-4:]}"
    return {
        "title": title,
        "description": f"{desc} over {service or 'iMessage'}",
        "dir": dirname,
    }


def _rewrite_frontmatter(md_path, info):
    text = md_path.read_text()
    if not text.startswith("---\n"):
        return
    end = text.index("\n---\n", 4) + len("\n---\n")
    md_path.write_text(_frontmatter(info) + text[end:])


def _migrate(out_dir, index, contacts):
    """Re-resolve names for every known conversation, renaming directories
    and refreshing frontmatter when a contact name (dis)appears."""
    for ident, entry in index.items():
        naming = _naming(
            ident, entry.get("display_name"), entry.get("service"),
            entry.get("participants", []), contacts, index,
        )
        if naming["dir"] == entry["dir"] and naming["title"] == entry["title"]:
            continue
        old = out_dir / "conversations" / entry["dir"]
        new = out_dir / "conversations" / naming["dir"]
        if old != new and old.exists():
            old.rename(new)
        entry.update(naming)
        entry["path"] = f"conversations/{entry['dir']}/messages.md"
        md_path = new / "messages.md"
        if md_path.exists():
            _rewrite_frontmatter(md_path, entry)


def _frontmatter(info):
    lines = ["---", "type: iMessage Conversation"]
    lines.append(f"title: {_yq(info['title'])}")
    lines.append(f"description: {_yq(info['description'])}")
    lines.append(f"resource: imessage://chat/{info['identifier']}")
    lines.append(f"chat_identifier: {_yq(info['identifier'])}")
    lines.append(f"service: {info['service']}")
    lines.append("participants:")
    for p in info["participants"]:
        lines.append(f"  - {_yq(p)}")
    lines.append("---")
    return "\n".join(lines) + "\n"


def _write_indexes(out_dir, index):
    conv_dir = out_dir / "conversations"
    conv_dir.mkdir(parents=True, exist_ok=True)
    _atomic_text(conv_dir / "index.json",
        json.dumps(index, indent=2, sort_keys=True, ensure_ascii=False) + "\n"
    )
    lines = [
        "---",
        "type: Directory Index",
        "title: 'Conversations'",
        "---",
        "",
        "# Conversations",
        "",
    ]
    for ident in sorted(index, key=lambda k: index[k]["title"].lower()):
        info = index[ident]
        who = ", ".join(info["participants"]) or ident
        lines.append(
            f"- [{info['title']}](/conversations/{info['dir']}/messages.md)"
            f" — {who}"
        )
    _atomic_text(conv_dir / "index.md", "\n".join(lines) + "\n")

    root = [
        "---",
        "okf_version: '0.2'",
        "type: Knowledge Bundle",
        "title: 'iMessage Archive'",
        "description: 'iMessage conversations synced from macOS, one directory per conversation.'",
        "---",
        "",
        "# iMessage Archive",
        "",
        f"{len(index)} conversations under [conversations/](/conversations/index.md).",
        "",
        "Each conversation is an append-only `messages.md`. Entries look like:",
        "",
        "```",
        "### [2026-08-26T14:32:05-04:00] me",
        "message text",
        "```",
        "",
        "`me` is the archive owner; other senders are phone numbers or email",
        "addresses. Body lines beginning with `### [` are escaped with `\\`.",
    ]
    _atomic_text(out_dir / "index.md", "\n".join(root) + "\n")


def build_link_tree(plan, staging_dir):
    """Stage (local_path, key) pairs as a symlink tree under staging_dir so
    one `aws s3 sync` can upload everything in parallel. Keys drop their
    leading 'conversations/' (the sync targets that prefix). Missing local
    files are skipped. Returns the number of links staged."""
    staging_dir = Path(staging_dir)
    staged = 0
    for path, key in plan:
        src = Path(path)
        if not src.exists():
            continue
        rel = key.removeprefix("conversations/")
        dest = staging_dir / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        if dest.is_symlink() or dest.exists():
            dest.unlink()
        dest.symlink_to(src)
        staged += 1
    return staged


def sync(db_path, out_dir, state_path, contacts=None, s3=None):
    """Append messages newer than the stored ROWID to their conversation
    files. Returns counts: {'messages': n, 'conversations': n}.

    s3 = {'bucket': str, 'uploader': callable(path, bucket, key) -> bool}
    enables attachment upload; failed uploads are retried on later runs via
    state['pending_uploads'] (references are written up front — keys are
    deterministic)."""
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    state = _load_json(state_path, {"last_rowid": 0})
    index = _load_json(out_dir / "conversations" / "index.json", {})

    pending = state.get("pending_uploads", [])
    if s3 and pending:
        still = []
        for item in pending:
            if not Path(item["path"]).exists():
                continue  # attachment deleted locally; give up
            if not s3["uploader"](item["path"], s3["bucket"], item["key"]):
                still.append(item)
        pending = still

    for ident, entry in index.items():
        entry.setdefault("dir", ident)
        if "display_name" not in entry:
            # legacy entries stored display_name only as the title
            entry["display_name"] = (
                entry["title"]
                if ident.startswith("chat") and entry["title"] != ident
                else None
            )
    renamed = False
    if contacts:
        before = {k: (e["dir"], e["title"]) for k, e in index.items()}
        _migrate(out_dir, index, contacts)
        renamed = any(
            (e["dir"], e["title"]) != before[k] for k, e in index.items()
        )

    con = sqlite3.connect(Path(db_path).resolve().as_uri() + "?mode=ro", uri=True)
    cur = con.cursor()
    rows = cur.execute(
        "SELECT m.ROWID, m.text, m.attributedBody, m.date, m.is_from_me,"
        " h.id, c.ROWID, c.chat_identifier, c.display_name, c.service_name,"
        " m.cache_has_attachments, m.associated_message_type"
        " FROM message m"
        " JOIN chat_message_join cmj ON cmj.message_id = m.ROWID"
        " JOIN chat c ON c.ROWID = cmj.chat_id"
        " LEFT JOIN handle h ON h.ROWID = m.handle_id"
        " WHERE m.ROWID > ?"
        " ORDER BY m.date, m.ROWID",
        (state["last_rowid"],),
    ).fetchall()

    written = 0
    touched = set()
    last_rowid = state["last_rowid"]

    for (rowid, text, blob, date, is_from_me, handle, chat_rowid,
         identifier, display_name, service, has_attach, assoc_type) in rows:
        last_rowid = max(last_rowid, rowid)
        if assoc_type not in _NORMAL_ASSOC_TYPES:
            continue

        body = text or decode_attributed_body(blob)
        attachments = _attachments(cur, rowid) if has_attach else []
        if not body and not attachments:
            continue

        ident = slugify(identifier)
        if ident not in index:
            participants = _participants(cur, chat_rowid)
            entry = {
                "identifier": ident,
                "display_name": display_name or None,
                "service": service or "iMessage",
                "participants": participants,
            }
            entry.update(_naming(
                ident, display_name, service, participants, contacts, index,
            ))
            entry["path"] = f"conversations/{entry['dir']}/messages.md"
            index[ident] = entry

        conv_dir = out_dir / "conversations" / index[ident]["dir"]
        md_path = conv_dir / "messages.md"
        if not md_path.exists():
            conv_dir.mkdir(parents=True, exist_ok=True)
            md_path.write_text(_frontmatter(index[ident]))

        attach_lines = []
        for att_id, mime, name, filename in attachments:
            line = f"[attachment: {mime or 'unknown'} {name or 'unnamed'}"
            local = Path(filename).expanduser() if filename else None
            if s3 and local and local.exists():
                key = attachment_key(index[ident]["dir"], att_id, name)
                line += f" s3://{s3['bucket']}/{key}"
                if not s3["uploader"](str(local), s3["bucket"], key):
                    pending.append({"path": str(local), "key": key})
            attach_lines.append(line + "]")

        sender = "me" if is_from_me else (handle or "unknown")
        when = apple_time_to_iso(date) or "unknown-time"
        parts = []
        if body:
            parts.append(escape_body(body))
        parts.extend(attach_lines)
        entry = f"\n{HEADER_PREFIX}{when}] {sender}\n" + "\n".join(parts) + "\n"
        with md_path.open("a") as f:
            f.write(entry)
        written += 1
        touched.add(ident)

    con.close()

    if written or renamed or not (out_dir / "index.md").exists():
        _write_indexes(out_dir, index)

    state["last_rowid"] = last_rowid
    state["pending_uploads"] = pending
    state["synced_at"] = datetime.now().astimezone().isoformat(timespec="seconds")
    Path(state_path).parent.mkdir(parents=True, exist_ok=True)
    _atomic_text(state_path, json.dumps(state, indent=2) + "\n")

    return {"messages": written, "conversations": len(touched)}
