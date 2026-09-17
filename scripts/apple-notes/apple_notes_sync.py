"""Sync Apple Notes from the macOS NoteStore database into an OKF v0.2
bundle designed for git to be the version history (#1057).

Reads NoteStore.sqlite strictly read-only. Unlike the iMessage bundle,
notes are mutable documents: a note's file is rewritten in place on edit,
moved on rename or folder change, and removed (with a tombstone in
`notes/index.json`) on deletion. Every body passes through `scrub.scrub`
before it is written (#1056).

Bundle layout:

    index.md                          okf_version: '0.2'
    notes/index.md                    human listing
    notes/index.json                  { "<uuid>": { title, folder, path, created,
                                        modified, redactions?, deleted? } }
    notes/<folder-slug>/<title-slug>.md
    .sync_state.json                  { "notes": { "<uuid>": { modified, sha256, path } },
                                        "skipped": { "<uuid>": { reason, modified } } }
"""
import gzip
import hashlib
import json
import os
import re
import sqlite3
import unicodedata
from datetime import datetime, timedelta, timezone
from pathlib import Path

from scrub import scrub

VERSION = "0.1"
APPLE_EPOCH = 978307200  # 2001-01-01 00:00:00 UTC in unix seconds
TOMBSTONE_DAYS = 30
FOLDER_TYPE_TRASH = 1
NOTE_TYPE = "Apple Note"
PLACEHOLDER = "￼"

_UTI_MIME = {
    "public.jpeg": "image/jpeg",
    "public.png": "image/png",
    "public.heic": "image/heic",
    "com.compuserve.gif": "image/gif",
    "public.tiff": "image/tiff",
    "com.adobe.pdf": "application/pdf",
    "public.mpeg-4": "video/mp4",
    "com.apple.quicktime-movie": "video/quicktime",
    "public.mp3": "audio/mpeg",
    "com.apple.m4a-audio": "audio/mp4",
    "public.plain-text": "text/plain",
}


def apple_time_to_iso(seconds):
    """Apple Core Data timestamp (seconds since 2001) → local ISO-8601 with offset."""
    dt = datetime.fromtimestamp(float(seconds) + APPLE_EPOCH, tz=timezone.utc).astimezone()
    return dt.isoformat(timespec="seconds")


def uti_to_mime(uti):
    return _UTI_MIME.get(uti, uti)


def slugify(title, limit=80):
    text = unicodedata.normalize("NFKD", title or "").encode("ascii", "ignore").decode()
    text = re.sub(r"[^A-Za-z0-9]+", "-", text).strip("-").lower()
    return text[:limit].rstrip("-") or "untitled"


# --- protobuf ----------------------------------------------------------------

def _varint(buf, i):
    result = shift = 0
    while True:
        if i >= len(buf):
            raise ValueError("truncated varint")
        byte = buf[i]
        i += 1
        result |= (byte & 0x7F) << shift
        shift += 7
        if not byte & 0x80:
            return result, i


def _walk(buf):
    """Yield (field_number, wire_type, value) for one protobuf message."""
    i = 0
    while i < len(buf):
        tag, i = _varint(buf, i)
        field, wire = tag >> 3, tag & 7
        if wire == 0:
            value, i = _varint(buf, i)
        elif wire == 2:
            length, i = _varint(buf, i)
            if i + length > len(buf):
                raise ValueError("truncated field")
            value = buf[i:i + length]
            i += length
        elif wire == 1:
            value, i = buf[i:i + 8], i + 8
        elif wire == 5:
            value, i = buf[i:i + 4], i + 4
        else:
            raise ValueError(f"unsupported wire type {wire}")
        yield field, wire, value


def _first(buf, field):
    for f, w, v in _walk(buf):
        if f == field and w == 2:
            return v
    return None


def decode_note_body(gz_data):
    """Return (plain_text, [(attachment_identifier, type_uti), ...]) from
    `ZICNOTEDATA.ZDATA`. Attachments are listed in the order of their U+FFFC
    placeholders in the text. Raises ValueError on undecodable data."""
    try:
        raw = gzip.decompress(gz_data)
    except (OSError, EOFError) as e:
        raise ValueError(f"not gzip: {e}")
    document = _first(raw, 2)
    note = _first(document, 3) if document is not None else None
    if note is None:
        raise ValueError("no note document in body")
    text = b""
    attachments = []
    for field, wire, value in _walk(note):
        if field == 2 and wire == 2:
            text = value
        elif field == 5 and wire == 2:
            info = _first(value, 12)
            if info is not None:
                ident = _first(info, 1)
                uti = _first(info, 2)
                attachments.append(((ident or b"").decode("utf-8", "replace"), (uti or b"").decode("utf-8", "replace")))
    return text.decode("utf-8", "replace"), attachments


# --- bundle writing ----------------------------------------------------------

def _yaml(value):
    # JSON strings are valid YAML double-quoted scalars, and handle `: # "`.
    return json.dumps(value, ensure_ascii=False)


def _frontmatter(meta):
    lines = ["---", f"type: {_yaml(NOTE_TYPE)}"]
    for key in ("identifier", "title", "folder", "account", "created", "modified"):
        lines.append(f"{key}: {_yaml(meta[key])}")
    for key in ("attachments", "redactions"):
        if meta.get(key):
            lines.append(f"{key}:")
            lines.extend(f"  - {_yaml(v)}" for v in meta[key])
    lines.append("---")
    return "\n".join(lines) + "\n\n"


def _load_json(path, default):
    path = Path(path)
    return json.loads(path.read_text()) if path.exists() else default


def _mkdir_private(path):
    """mkdir -p with 0o700 on every created level (mkdir's `mode` only
    applies to the leaf, so intermediates would otherwise follow the umask)."""
    path = Path(path)
    missing = []
    while not path.exists():
        missing.append(path)
        path = path.parent
    for directory in reversed(missing):
        directory.mkdir(mode=0o700)
        os.chmod(directory, 0o700)


def _write_private(path, text):
    path = Path(path)
    _mkdir_private(path.parent)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        f.write(text)


def _write_indexes(out_dir, index):
    live = sorted((e for e in index.values() if "path" in e), key=lambda e: (e["folder"].lower(), e["title"].lower()))
    lines = ["---", "type: Directory Listing", "title: Apple Notes", f"count: {len(live)}", "---", ""]
    folder = None
    for entry in live:
        if entry["folder"] != folder:
            folder = entry["folder"]
            lines += [f"## {folder}", ""]
        lines.append(f"- [{entry['title']}]({entry['path']}) — modified {entry['modified']}")
    _write_private(out_dir / "notes" / "index.md", "\n".join(lines) + "\n")
    _write_private(out_dir / "notes" / "index.json", json.dumps(index, indent=2, ensure_ascii=False, sort_keys=True) + "\n")
    root = [
        "---", "okf_version: '0.2'", "type: Knowledge Bundle", "title: Apple Notes",
        f"generator: apple_notes_sync {VERSION}", "---", "",
        "Apple Notes exported as one Markdown file per note under `notes/<folder>/`.",
        "See `notes/index.md` for the listing and `notes/index.json` for machine-readable metadata.",
        "",
        "Rules for consumers:", "",
        "- Files are rewritten in place when a note is edited; git history is the version history.",
        "- `identifier` (frontmatter and `index.json` key) is the stable note id; paths change on rename.",
        "- A deleted note has no file; its `index.json` entry carries `deleted: <ISO time>` for 30 days.",
        "- `[REDACTED:<kind>]` marks a scrubbed secret; `redactions:` lists the kinds present.",
        "- `[attachment: <mime> <filename>]` marks an inline attachment.",
    ]
    _write_private(out_dir / "index.md", "\n".join(root) + "\n")


def _rows(con):
    return con.execute(
        """
        SELECT n.Z_PK, n.ZIDENTIFIER, n.ZTITLE1,
               COALESCE(n.ZCREATIONDATE1, n.ZCREATIONDATE3, n.ZCREATIONDATE2, n.ZCREATIONDATE),
               COALESCE(n.ZMODIFICATIONDATE1, n.ZMODIFICATIONDATE),
               f.ZTITLE2, f.ZFOLDERTYPE, a.ZNAME, n.ZISPASSWORDPROTECTED, n.ZMARKEDFORDELETION
        FROM ZICCLOUDSYNCINGOBJECT n
        LEFT JOIN ZICCLOUDSYNCINGOBJECT f ON f.Z_PK = n.ZFOLDER
        LEFT JOIN ZICCLOUDSYNCINGOBJECT a ON a.Z_PK = n.ZACCOUNT7
        WHERE n.ZTITLE1 IS NOT NULL AND n.ZIDENTIFIER IS NOT NULL
        ORDER BY n.ZIDENTIFIER
        """
    ).fetchall()


def _attachment_names(con, note_pk):
    """{attachment_identifier: filename} for a note's attachment objects."""
    out = {}
    for ident, filename in con.execute(
        """
        SELECT att.ZIDENTIFIER, COALESCE(media.ZFILENAME, att.ZFILENAME)
        FROM ZICCLOUDSYNCINGOBJECT att
        LEFT JOIN ZICCLOUDSYNCINGOBJECT media ON media.Z_PK = att.ZMEDIA
        WHERE att.ZNOTE = ? AND att.ZTYPEUTI IS NOT NULL
        """,
        (note_pk,),
    ):
        out[ident] = filename
    return out


def _first_line(con, note_pk):
    row = con.execute("SELECT ZDATA FROM ZICNOTEDATA WHERE ZNOTE = ?", (note_pk,)).fetchone()
    if not row or not row[0]:
        return ""
    try:
        text, _ = decode_note_body(row[0])
    except ValueError:
        return ""
    return text.split("\n", 1)[0]


def _render_body(text, attachments, names):
    """Replace U+FFFC placeholders with attachment lines. Returns (text, labels)."""
    labels = []
    parts = text.split(PLACEHOLDER)
    rendered = parts[0]
    for i, part in enumerate(parts[1:]):
        if i < len(attachments):
            ident, uti = attachments[i]
            label = f"{uti_to_mime(uti)} {names.get(ident) or ident}"
        else:
            label = "unknown"
        labels.append(label)
        rendered += f"[attachment: {label}]" + part
    return rendered, labels


def _note_path(folder, title, uuid, taken):
    base = f"notes/{slugify(folder)}/{slugify(title)}"
    path = base + ".md"
    if path in taken:
        path = f"{base}-{uuid[:8].lower()}.md"
    return path


def sync(db_path, out_dir, state_path, config=None, touched=None):
    """Bring `out_dir` in line with the Notes database. Returns counts:
    {'new', 'updated', 'renamed', 'deleted', 'unchanged', 'skipped'}.
    If `touched` is a list, the (scrubbed) titles of written or removed
    notes are appended to it, for commit messages."""
    config = config or {}
    touched = touched if touched is not None else []
    skip_folders = set(config.get("skip_folders") or [])
    skip_notes = set(config.get("skip_notes") or [])
    out_dir = Path(out_dir)
    _mkdir_private(out_dir)
    state = _load_json(state_path, {"notes": {}, "skipped": {}})
    state.setdefault("notes", {})
    state.setdefault("skipped", {})
    index = _load_json(out_dir / "notes" / "index.json", {})
    loaded = json.dumps(state, sort_keys=True), json.dumps(index, sort_keys=True)
    counts = {k: 0 for k in ("new", "updated", "renamed", "deleted", "unchanged", "skipped")}
    now = datetime.now(timezone.utc).astimezone().isoformat(timespec="seconds")

    con = sqlite3.connect(f"file:{Path(db_path).resolve()}?mode=ro", uri=True)
    try:
        live = {}
        for pk, uuid, title, created, modified, folder, ftype, account, locked, marked in _rows(con):
            if marked or ftype == FOLDER_TYPE_TRASH or locked:
                continue
            # A note with no usable date at all (seen in the wild) still
            # exports; it just takes the other date, else the run time.
            modified = modified if modified is not None else created
            created = created if created is not None else modified
            if modified is None:
                created = modified = datetime.now(timezone.utc).timestamp() - APPLE_EPOCH
            live[uuid] = (pk, title, created, modified, folder or "", account or "")

        # Tombstones for notes that vanished (deleted, trashed, or locked since).
        for uuid in list(state["notes"]):
            if uuid in live:
                continue
            old = state["notes"].pop(uuid)
            _remove(out_dir, old.get("path"))
            entry = index.get(uuid, {})
            touched.append(f"deleted: {entry.get('title', uuid)}")
            for key in ("path", "redactions"):
                entry.pop(key, None)
            entry["deleted"] = now
            index[uuid] = entry
            counts["deleted"] += 1
        for uuid in list(state["skipped"]):
            if uuid not in live:
                state["skipped"].pop(uuid)
                index.pop(uuid, None)

        taken = set()
        for uuid, (pk, title, created, modified, folder, account) in live.items():
            reason = None
            if folder in skip_folders:
                reason = "skip-folder"
            elif uuid in skip_notes:
                reason = "skip-note"
            elif scrub(title)[1]:
                reason = "title-secret"
            prior = state["skipped"].get(uuid) if reason is None else None
            if reason is None and not (prior and prior.get("modified") == modified):
                # The stored title is a truncated first line; a secret cut
                # mid-way escapes the title scan but its prefix would still
                # name the file. Scan the real first line of the body.
                first = _first_line(con, pk)
                if first and scrub(first)[1]:
                    reason = "title-secret"
            if reason:
                prior = state["skipped"].get(uuid)
                if not prior or prior.get("modified") != modified or prior.get("reason") != reason:
                    state["skipped"][uuid] = {"reason": reason, "modified": modified}
                if uuid in state["notes"]:
                    _remove(out_dir, state["notes"].pop(uuid).get("path"))
                index.pop(uuid, None)
                counts["skipped"] += 1
                continue
            state["skipped"].pop(uuid, None)

            prior = state["notes"].get(uuid)
            path = _note_path(folder, title, uuid, taken)
            taken.add(path)
            if prior and prior.get("modified") == modified and prior.get("path") == path:
                counts["unchanged"] += 1
                continue

            row = con.execute("SELECT ZDATA FROM ZICNOTEDATA WHERE ZNOTE = ?", (pk,)).fetchone()
            try:
                text, attachments = decode_note_body(row[0]) if row and row[0] else ("", [])
            except ValueError:
                text, attachments = "", []
            text, labels = _render_body(text, attachments, _attachment_names(con, pk))
            text, findings = scrub(text)
            redactions = sorted({f.kind for f in findings})
            digest = hashlib.sha256(f"{title}\n{text}".encode("utf-8")).hexdigest()

            if prior and prior.get("sha256") == digest and prior.get("path") == path:
                counts["unchanged"] += 1  # modification date bumped, content identical
                continue

            meta = {
                "identifier": uuid, "title": title, "folder": folder, "account": account,
                "created": apple_time_to_iso(created), "modified": apple_time_to_iso(modified),
                "attachments": labels, "redactions": redactions,
            }
            if prior and prior.get("path") and prior["path"] != path:
                _remove(out_dir, prior["path"])
                counts["renamed"] += 1
            elif prior:
                counts["updated"] += 1
            else:
                counts["new"] += 1
            body = text if not text or text.endswith("\n") else text + "\n"
            _write_private(out_dir / path, _frontmatter(meta) + body)
            touched.append(scrub(title)[0])
            entry = {"title": title, "folder": folder, "path": path,
                     "created": meta["created"], "modified": meta["modified"]}
            if redactions:
                entry["redactions"] = redactions
            index[uuid] = entry
            state["notes"][uuid] = {"modified": modified, "sha256": digest, "path": path}
    finally:
        con.close()

    cutoff = datetime.now(timezone.utc) - timedelta(days=TOMBSTONE_DAYS)
    for uuid, entry in list(index.items()):
        if "deleted" in entry and datetime.fromisoformat(entry["deleted"]) < cutoff:
            index.pop(uuid)

    if json.dumps(index, sort_keys=True) != loaded[1] or not (out_dir / "index.md").exists():
        _write_indexes(out_dir, index)
    if json.dumps(state, sort_keys=True) != loaded[0] or not Path(state_path).exists():
        _write_private(state_path, json.dumps(state, indent=2, sort_keys=True) + "\n")
    return counts


def _remove(out_dir, rel):
    if not rel:
        return
    path = Path(out_dir) / rel
    if path.exists():
        path.unlink()
        parent = path.parent
        if parent != Path(out_dir) / "notes" and not any(parent.iterdir()):
            parent.rmdir()
