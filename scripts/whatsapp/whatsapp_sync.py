"""Sync WhatsApp conversations from WhatsApp Desktop's local database
(ChatStorage.sqlite) into an OKF v0.2 bundle — same layout and parsing
contract as iMessageSync.

Reads a consistent SQLite snapshot (the app holds the original open), tracks the
last synced message Z_PK in `.sync_state.json`, appends only new messages.
"""
import json
import re
import hashlib
import os
import sqlite3
import tempfile
from datetime import datetime, timezone
from pathlib import Path

APPLE_EPOCH = 978307200  # Core Data epoch: 2001-01-01 UTC

HEADER_PREFIX = "### ["

# ZSESSIONTYPE: 0 = DM, 1 = group; >= 2 = broadcast/status noise.
_SYNCED_SESSION_TYPES = (0, 1)
# ZMESSAGETYPE 6 = group event, 10 = system notice, 14 = deletion pointer
# (its media item's title is the removed message's id). ZTEXT on these holds
# a JID, never anything a person wrote.
SYSTEM_MESSAGE_TYPES = (6, 10, 14)

# A source app that stops running freezes its database, and an exporter with
# nothing to export looks identical to a healthy one. Report the age so a cold
# source is visible in the job's own log instead of being silent for weeks.
SOURCE_STALE_AFTER_S = 6 * 3600


def source_staleness(db_path, now=None, threshold=SOURCE_STALE_AFTER_S):
    """Seconds since `db_path` was written when that exceeds `threshold`.

    None when the source is fresh or unreadable — the caller reports staleness,
    it does not diagnose a missing file.
    """
    import time
    try:
        age = (now if now is not None else time.time()) - os.path.getmtime(db_path)
    except OSError:
        return None
    return age if age > threshold else None


def core_data_time_to_iso(raw):
    """ZMESSAGEDATE is seconds since 2001-01-01 UTC."""
    if not raw:
        return None
    dt = datetime.fromtimestamp(raw + APPLE_EPOCH, tz=timezone.utc)
    return dt.astimezone().isoformat(timespec="seconds")


def newer_iso(current, candidate):
    """The later of two ISO-8601 timestamps, compared as instants so differing
    UTC offsets order correctly. Unparseable values ('unknown-time') never win,
    so an incremental run cannot regress a conversation's freshness cursor."""
    def instant(value):
        try:
            return datetime.fromisoformat(value)
        except (TypeError, ValueError):
            return None

    a, b = instant(current), instant(candidate)
    if b is None:
        return current
    if a is None:
        return candidate
    return candidate if b > a else current


def jid_handle(jid):
    """`14155550123@s.whatsapp.net` -> `+14155550123`; group/broadcast JIDs
    pass through unchanged."""
    if not jid:
        return None
    local, _, domain = jid.partition("@")
    if domain == "s.whatsapp.net" and local.isdigit():
        return f"+{local}"
    return jid


_TITLE_JUNK = re.compile(r"[‎‏‪-‮⁦-⁩]")


def clean_title(name):
    """ZPARTNERNAME embeds bidi isolates and non-breaking spaces."""
    if not name:
        return ""
    return _TITLE_JUNK.sub("", name).replace(" ", " ").strip()


# Desktop keeps an encoded '+<base64>' token in ZWAGROUPMEMBER.ZFIRSTNAME
# for some members; it is never a name a person would recognise.
_ENCODED_NAME = re.compile(r"\+(?=[A-Za-z0-9+/]*[A-Za-z])[A-Za-z0-9+/]+={0,2}")


def member_name(name):
    name = clean_title(name)
    return "" if _ENCODED_NAME.fullmatch(name) else name


def escape_body(text):
    return "\n".join(
        "\\" + line if line.startswith(HEADER_PREFIX) else line
        for line in text.split("\n")
    )


def slugify(name):
    slug = re.sub(r"[^A-Za-z0-9+@._-]", "_", name)
    slug = re.sub(r"_+", "_", slug).strip("_")
    if slug in ("", ".", ".."):
        return "chat-" + hashlib.sha256(name.encode()).hexdigest()[:16]
    return slug


_PHONE_SHAPED = re.compile(r"^[\d\s()+.‐-―-]*$")


def display_title(partner_name, handle):
    """Contact names pass through; a ZPARTNERNAME that is just a formatted
    phone number ('+1 (555) 207-2258') collapses to the E.164 handle."""
    title = clean_title(partner_name)
    if title and not _PHONE_SHAPED.match(title):
        return title
    return handle or title


def _yq(value):
    return "'" + str(value).replace("'", "''") + "'"


def _load_json(path, default):
    path = Path(path)
    return json.loads(path.read_text()) if path.exists() else default


def _frontmatter(info):
    lines = [
        "---",
        "type: WhatsApp Conversation",
        f"title: {_yq(info['title'])}",
        f"description: {_yq(info['description'])}",
        f"resource: whatsapp://chat/{info['identifier']}",
        f"chat_identifier: {_yq(info['identifier'])}",
        "service: WhatsApp",
        "participants:",
    ]
    for p in info["participants"]:
        lines.append(f"  - {_yq(p)}")
    lines.append("---")
    return "\n".join(lines) + "\n"


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
    _atomic_text(conv_dir / "index.md","\n".join(lines) + "\n")

    root = [
        "---",
        "okf_version: '0.2'",
        "type: Knowledge Bundle",
        "title: 'WhatsApp Archive'",
        "description: 'WhatsApp conversations synced from WhatsApp Desktop, one directory per conversation.'",
        "---",
        "",
        "# WhatsApp Archive",
        "",
        f"{len(index)} conversations under [conversations/](/conversations/index.md).",
        "",
        "Same parsing contract as iMessageSync: entries are",
        "`### [ISO-8601 offset] <sender>`, `me` is the archive owner, other",
        "senders are E.164 phone numbers. Body lines beginning with `### [`",
        "are escaped with `\\`.",
    ]
    _atomic_text(out_dir / "index.md","\n".join(root) + "\n")


def _sessions(cur):
    out = {}
    for pk, jid, name, stype in cur.execute(
        "SELECT Z_PK, ZCONTACTJID, ZPARTNERNAME, ZSESSIONTYPE FROM ZWACHATSESSION"
    ):
        if jid == "status@broadcast" or (stype not in _SYNCED_SESSION_TYPES):
            continue
        out[pk] = {"jid": jid, "raw_name": name, "type": stype}
    return out


def _columns(cur, table):
    return {r[1] for r in cur.execute(f"PRAGMA table_info({table})")}


def _readonly(path):
    return sqlite3.connect(Path(path).resolve().as_uri() + "?mode=ro", uri=True)


def load_identities(container):
    """`<id>@lid` -> E.164 handle, from the stores WhatsApp Desktop keeps
    next to ChatStorage.sqlite (#1762). Missing or unexpected stores yield
    nothing: an unmapped `@lid` stays raw, it is never guessed."""
    container = Path(container)
    out = {}
    sources = [
        ("LID.sqlite", "ZWAZACCOUNT", "ZIDENTIFIER", "ZPHONENUMBER"),
        ("ContactsV2.sqlite", "ZWAADDRESSBOOKCONTACT", "ZLID", "ZWHATSAPPID"),
    ]
    for name, table, lid_col, phone_col in sources:
        path = container / name
        if not path.exists():
            continue
        try:
            con = _readonly(path)
            try:
                rows = con.execute(
                    f"SELECT {lid_col}, {phone_col} FROM {table}"
                ).fetchall()
            finally:
                con.close()
        except sqlite3.Error:
            continue
        for lid, phone in rows:
            if not lid or not str(lid).endswith("@lid") or not phone:
                continue
            digits = str(phone).split("@", 1)[0].lstrip("+")
            if digits.isdigit() and 7 <= len(digits) <= 15:
                out.setdefault(lid, f"+{digits}")
    return out


def _group_members(cur):
    """{group session pk: [(handle, display name or None, active)]}.
    Empty when the database predates the session link."""
    cols = _columns(cur, "ZWAGROUPMEMBER")
    if "ZCHATSESSION" not in cols:
        return {}
    first = "ZFIRSTNAME" if "ZFIRSTNAME" in cols else "NULL"
    active = "COALESCE(ZISACTIVE, 1) = 1" if "ZISACTIVE" in cols else "1"
    push = {}
    if "ZJID" in _columns(cur, "ZWAPROFILEPUSHNAME"):
        push = dict(cur.execute("SELECT ZJID, ZPUSHNAME FROM ZWAPROFILEPUSHNAME"))
    out = {}
    for session, jid, contact, given, is_active in cur.execute(
        f"SELECT ZCHATSESSION, ZMEMBERJID, ZCONTACTNAME, {first}, {active}"
        " FROM ZWAGROUPMEMBER WHERE ZMEMBERJID IS NOT NULL"
    ):
        name = member_name(contact) or member_name(given) or member_name(push.get(jid))
        out.setdefault(session, []).append((jid_handle(jid), name or None, bool(is_active)))
    return out


def _newest_header_time(md_path):
    """Latest entry timestamp already written to a conversation file."""
    newest = None
    try:
        with open(md_path, encoding="utf-8") as f:
            for line in f:
                if line.startswith(HEADER_PREFIX) and "] " in line:
                    newest = newer_iso(newest, line[len(HEADER_PREFIX):line.index("] ")])
    except OSError:
        return None
    return newest


def _refresh_metadata(out_dir, index, sessions, members, identities):
    """Group members, their names, and the `@lid` → phone map, recomputed on
    every run so existing entries gain them without new messages."""
    by_jid = {s["jid"]: pk for pk, s in sessions.items()}
    for ident, entry in index.items():
        if not entry.get("newest_entry"):
            newest = _newest_header_time(out_dir / entry["path"])
            if newest:
                entry["newest_entry"] = newest
        pk = by_jid.get(ident)
        if ident.endswith("@g.us") and pk in members:
            people = [m for m in members[pk] if m[2]]
            # Every member inactive: the owner left. Keep who was there, and
            # say so, so replies are never drafted into a group they left.
            if people:
                entry.pop("active", None)
            else:
                people = members[pk]
                entry["active"] = False
            entry["participants"] = sorted({h for h, _, _ in people})
            names = {h: n for h, n, _ in people if n}
            if names:
                entry["participant_names"] = dict(sorted(names.items()))
        lids = {p for p in entry.get("participants", []) if p.endswith("@lid")}
        if ident.endswith("@lid"):
            lids.add(ident)
        identity = {l: identities[l] for l in sorted(lids) if l in identities}
        if identity:
            entry["identity"] = identity


def _group_member_handle(cur, member_pk):
    row = cur.execute(
        "SELECT ZMEMBERJID FROM ZWAGROUPMEMBER WHERE Z_PK = ?", (member_pk,)
    ).fetchone()
    return jid_handle(row[0]) if row and row[0] else None


def _media_note(cur, media_pk):
    row = cur.execute(
        "SELECT ZMEDIALOCALPATH, ZTITLE FROM ZWAMEDIAITEM WHERE Z_PK = ?",
        (media_pk,),
    ).fetchone()
    if not row:
        return None
    path, title = row
    name = title or (Path(path).name if path else None) or "file"
    return f"[attachment: whatsapp-media {name}]"


def sync(db_path, out_dir, state_path):
    """Append messages newer than the stored Z_PK to their conversation
    files. Copies the database first — WhatsApp Desktop holds the original
    open. Returns {'messages': n, 'conversations': n}."""
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    state = _load_json(state_path, {"last_pk": 0})
    loaded_pk = state.get("last_pk")
    index = _load_json(out_dir / "conversations" / "index.json", {})
    loaded_index = json.dumps(index, sort_keys=True)
    identities = load_identities(Path(db_path).resolve().parent)

    with tempfile.TemporaryDirectory() as td:
        snap = Path(td) / "ChatStorage.sqlite"
        source = sqlite3.connect(Path(db_path).resolve().as_uri() + "?mode=ro", uri=True)
        con = sqlite3.connect(snap)
        try:
            source.backup(con)
        finally:
            source.close()
        cur = con.cursor()
        sessions = _sessions(cur)

        kind = "ZMESSAGETYPE" if "ZMESSAGETYPE" in _columns(cur, "ZWAMESSAGE") else "0"
        rows = cur.execute(
            "SELECT Z_PK, ZCHATSESSION, ZTEXT, ZMESSAGEDATE, ZISFROMME,"
            f" ZFROMJID, ZGROUPMEMBER, ZMEDIAITEM, {kind}"
            " FROM ZWAMESSAGE WHERE Z_PK > ? ORDER BY ZMESSAGEDATE, Z_PK",
            (state["last_pk"],),
        ).fetchall()

        written = 0
        touched = set()
        last_pk = state["last_pk"]

        for (pk, session_pk, text, date, from_me, from_jid, member_pk,
             media_pk, message_type) in rows:
            last_pk = max(last_pk, pk)
            sess = sessions.get(session_pk)
            if sess is None or message_type in SYSTEM_MESSAGE_TYPES:
                continue

            parts = []
            if text:
                parts.append(escape_body(text))
            if media_pk:
                note = _media_note(cur, media_pk)
                if note:
                    parts.append(note)
            if not parts:
                continue  # system/group events carry no content

            ident = sess["jid"]
            if ident not in index:
                is_group = sess["type"] == 1 or ident.endswith("@g.us")
                handle = jid_handle(ident)
                participants = [] if is_group else [handle]
                title = display_title(sess["raw_name"], handle) or ident
                dirname = slugify(title)
                if not re.search(r"[A-Za-z0-9]", dirname):
                    dirname = slugify(ident)
                taken = {e["dir"] for k, e in index.items() if k != ident}
                if dirname in taken:
                    dirname = f"{dirname}-{hashlib.sha256(ident.encode()).hexdigest()}"
                desc = (
                    f"Group conversation '{title}'" if is_group
                    else f"Conversation with {title}"
                ) + " over WhatsApp"
                index[ident] = {
                    "identifier": ident,
                    "title": title,
                    "description": desc,
                    "service": "WhatsApp",
                    "participants": participants,
                    "dir": dirname,
                    "path": f"conversations/{dirname}/messages.md",
                }

            conv_dir = out_dir / "conversations" / index[ident]["dir"]
            md_path = conv_dir / "messages.md"
            if not md_path.exists():
                conv_dir.mkdir(parents=True, exist_ok=True)
                md_path.write_text(_frontmatter(index[ident]))

            if from_me:
                sender = "me"
            elif member_pk:
                sender = (
                    _group_member_handle(cur, member_pk)
                    or jid_handle(from_jid)
                    or "unknown"
                )
            else:
                sender = jid_handle(from_jid) or jid_handle(ident) or "unknown"

            when = core_data_time_to_iso(date) or "unknown-time"
            entry = f"\n{HEADER_PREFIX}{when}] {sender}\n" + "\n".join(parts) + "\n"
            with md_path.open("a") as f:
                f.write(entry)
            # #1429 — recall reads this to tell "no match" from "the archive
            # stopped here"; it must come from a message, never from synced_at.
            cursor = newer_iso(index[ident].get("newest_entry"), when)
            if cursor is not None:
                index[ident]["newest_entry"] = cursor
            written += 1
            touched.add(ident)

        _refresh_metadata(out_dir, index, sessions, _group_members(cur), identities)
        con.close()

    changed = json.dumps(index, sort_keys=True) != loaded_index
    if written or changed or not (out_dir / "index.md").exists():
        _write_indexes(out_dir, index)

    # Only touch the state file when the cursor advanced. A Git-backed bundle
    # (`sync.py --commit` runs `git add -A`) would otherwise commit a synced_at
    # bump on every scheduled run, which makes a dead source look healthy.
    if loaded_pk != last_pk or not Path(state_path).exists():
        state["last_pk"] = last_pk
        state["synced_at"] = datetime.now().astimezone().isoformat(timespec="seconds")
        Path(state_path).parent.mkdir(parents=True, exist_ok=True)
        _atomic_text(state_path, json.dumps(state, indent=2) + "\n")
    return {"messages": written, "conversations": len(touched)}
