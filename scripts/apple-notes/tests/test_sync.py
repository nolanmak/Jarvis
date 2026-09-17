"""Tests for apple_notes_sync (#1057).

The fixture NoteStore.sqlite carries only the subset of Apple's schema the
sync reads, and note bodies are built by a tiny protobuf encoder, so tests
need neither a Mac nor anyone's real notes.
"""
import gzip
import hashlib
import json
import os
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
import unittest.mock
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from apple_notes_sync import (  # noqa: E402
    APPLE_EPOCH,
    apple_time_to_iso,
    decode_note_body,
    slugify,
    sync,
    uti_to_mime,
)

ACCOUNT_PK = 2
FOLDER_NOTES = 3
FOLDER_WORK = 4
FOLDER_TRASH = 1


# --- minimal protobuf encoder ------------------------------------------------

def _varint(n):
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        if n:
            out.append(b | 0x80)
        else:
            out.append(b)
            return bytes(out)


def _field(num, payload):
    if isinstance(payload, int):
        return _varint(num << 3) + _varint(payload)
    return _varint((num << 3) | 2) + _varint(len(payload)) + payload


def encode_body(text, attachments=()):
    """attachments: [(identifier, type_uti), ...] in the order their U+FFFC
    placeholders appear in text. Mirrors the real layout: top.2 = document,
    document.3 = note, note.2 = text, note.5 = attribute runs, run.12 =
    attachment_info{1: identifier, 2: type_uti}."""
    runs = b""
    for ident, uti in attachments:
        info = _field(1, ident.encode()) + _field(2, uti.encode())
        runs += _field(5, _field(1, 1) + _field(12, info))
    note = _field(2, text.encode("utf-8")) + runs
    doc = _field(1, 0) + _field(2, 0) + _field(3, note)
    return gzip.compress(_field(1, 0) + _field(2, doc))


# --- fixture database --------------------------------------------------------

def make_fixture_db(path):
    con = sqlite3.connect(path)
    con.executescript(
        """
        CREATE TABLE ZICCLOUDSYNCINGOBJECT (
            Z_PK INTEGER PRIMARY KEY,
            ZTITLE1 TEXT, ZTITLE2 TEXT, ZNAME TEXT, ZIDENTIFIER TEXT,
            ZFOLDER INTEGER, ZACCOUNT7 INTEGER, ZFOLDERTYPE INTEGER,
            ZISPASSWORDPROTECTED INTEGER, ZMARKEDFORDELETION INTEGER,
            ZCREATIONDATE REAL, ZCREATIONDATE1 REAL, ZCREATIONDATE2 REAL, ZCREATIONDATE3 REAL,
            ZMODIFICATIONDATE REAL, ZMODIFICATIONDATE1 REAL,
            ZNOTE INTEGER, ZTYPEUTI TEXT, ZFILENAME TEXT, ZMEDIA INTEGER
        );
        CREATE TABLE ZICNOTEDATA (
            Z_PK INTEGER PRIMARY KEY, ZNOTE INTEGER, ZDATA BLOB
        );
        """
    )
    con.execute(
        "INSERT INTO ZICCLOUDSYNCINGOBJECT (Z_PK, ZNAME, ZIDENTIFIER) VALUES (?, ?, ?)",
        (ACCOUNT_PK, "iCloud", "4A43B10C-0000-0000-0000-000000000000"),
    )
    for pk, title, ftype in ((FOLDER_TRASH, "Recently Deleted", 1), (FOLDER_NOTES, "Notes", 0), (FOLDER_WORK, "Work", 0)):
        con.execute(
            "INSERT INTO ZICCLOUDSYNCINGOBJECT (Z_PK, ZTITLE2, ZFOLDERTYPE, ZMARKEDFORDELETION, ZACCOUNT7) VALUES (?, ?, ?, 0, ?)",
            (pk, title, ftype, ACCOUNT_PK),
        )
    con.commit()
    return con


def apple(unix_seconds):
    return unix_seconds - APPLE_EPOCH


def add_note(con, pk, title, text, *, folder=FOLDER_NOTES, created=1782475200, modified=None,
             uuid=None, locked=0, deleted=0, attachments=()):
    modified = created if modified is None else modified
    uuid = uuid or f"00000000-0000-0000-0000-{pk:012d}"
    con.execute(
        "INSERT INTO ZICCLOUDSYNCINGOBJECT (Z_PK, ZTITLE1, ZIDENTIFIER, ZFOLDER, ZACCOUNT7, "
        "ZISPASSWORDPROTECTED, ZMARKEDFORDELETION, ZCREATIONDATE1, ZMODIFICATIONDATE1) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (pk, title, uuid, folder, ACCOUNT_PK, locked, deleted, apple(created), apple(modified)),
    )
    con.execute("INSERT INTO ZICNOTEDATA (ZNOTE, ZDATA) VALUES (?, ?)", (pk, encode_body(text, attachments)))
    con.commit()
    return uuid


def add_attachment(con, note_pk, ident, uti, filename, pk=None):
    pk = pk or 1000 + con.execute("SELECT count(*) FROM ZICCLOUDSYNCINGOBJECT").fetchone()[0]
    media_pk = pk + 1
    con.execute(
        "INSERT INTO ZICCLOUDSYNCINGOBJECT (Z_PK, ZIDENTIFIER, ZFILENAME) VALUES (?, ?, ?)",
        (media_pk, f"MEDIA-{ident}", filename),
    )
    con.execute(
        "INSERT INTO ZICCLOUDSYNCINGOBJECT (Z_PK, ZNOTE, ZIDENTIFIER, ZTYPEUTI, ZMEDIA, ZMARKEDFORDELETION) VALUES (?, ?, ?, ?, ?, 0)",
        (pk, note_pk, ident, uti, media_pk),
    )
    con.commit()


def set_body(con, pk, text, modified, attachments=()):
    con.execute("UPDATE ZICNOTEDATA SET ZDATA = ? WHERE ZNOTE = ?", (encode_body(text, attachments), pk))
    con.execute("UPDATE ZICCLOUDSYNCINGOBJECT SET ZMODIFICATIONDATE1 = ? WHERE Z_PK = ?", (apple(modified), pk))
    con.commit()


# --- unit tests --------------------------------------------------------------

class HelperTests(unittest.TestCase):
    def test_apple_time_to_iso_has_offset(self):
        iso = apple_time_to_iso(apple(1782475200))
        self.assertRegex(iso, r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}[+-]\d{2}:\d{2}$")

    def test_slugify(self):
        self.assertEqual(slugify("Hello, World!  2026"), "hello-world-2026")
        self.assertEqual(slugify("https://example.com/a/b"), "https-example-com-a-b")
        self.assertEqual(slugify("📫"), "untitled")
        self.assertEqual(slugify(""), "untitled")
        self.assertEqual(len(slugify("x" * 500)), 80)

    def test_uti_to_mime(self):
        self.assertEqual(uti_to_mime("public.jpeg"), "image/jpeg")
        self.assertEqual(uti_to_mime("public.png"), "image/png")
        self.assertEqual(uti_to_mime("com.adobe.pdf"), "application/pdf")
        self.assertEqual(uti_to_mime("com.apple.notes.table"), "com.apple.notes.table")


class DecodeTests(unittest.TestCase):
    def test_round_trips_text_and_attachment_order(self):
        text = "Heading\n• bullet one\n• bullet two\n￼\nemoji 🎉 done\n￼"
        atts = [("A-1", "public.jpeg"), ("A-2", "com.adobe.pdf")]
        got_text, got_atts = decode_note_body(encode_body(text, atts))
        self.assertEqual(got_text, text)
        self.assertEqual(got_atts, atts)

    def test_empty_note(self):
        self.assertEqual(decode_note_body(encode_body("")), ("", []))

    def test_garbage_raises_value_error(self):
        with self.assertRaises(ValueError):
            decode_note_body(gzip.compress(b"\xff\xff\xff"))


# --- sync tests --------------------------------------------------------------

class SyncTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="apple notes test ")
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.db = root / "NoteStore.sqlite"
        self.con = make_fixture_db(self.db)
        self.addCleanup(self.con.close)
        self.out = root / "bundle"
        self.state = self.out / ".sync_state.json"

    def run_sync(self, **cfg):
        return sync(self.db, self.out, self.state, config=cfg)

    def read(self, rel):
        return (self.out / rel).read_text()

    def index(self):
        return json.loads(self.read("notes/index.json"))

    def state_json(self):
        return json.loads(self.state.read_text())

    # -- creation --

    def test_new_note_written_with_frontmatter_index_and_state(self):
        uuid = add_note(self.con, 10, "Grocery list", "eggs\nmilk", created=1782475200, modified=1782475300)
        result = self.run_sync()
        self.assertEqual(result["new"], 1)
        md = self.read("notes/notes/grocery-list.md")
        self.assertTrue(md.startswith("---\n"))
        self.assertIn('type: "Apple Note"', md)
        self.assertIn(f'identifier: "{uuid}"', md)
        self.assertIn('title: "Grocery list"', md)
        self.assertIn('folder: "Notes"', md)
        self.assertIn('account: "iCloud"', md)
        self.assertIn("created: ", md)
        self.assertIn("modified: ", md)
        self.assertNotIn("redactions:", md)
        self.assertTrue(md.endswith("\n---\n\neggs\nmilk\n"))
        entry = self.index()[uuid]
        self.assertEqual(entry["title"], "Grocery list")
        self.assertEqual(entry["folder"], "Notes")
        self.assertEqual(entry["path"], "notes/notes/grocery-list.md")
        self.assertNotIn("deleted", entry)
        st = self.state_json()["notes"][uuid]
        self.assertEqual(st["path"], "notes/notes/grocery-list.md")
        self.assertEqual(st["sha256"], hashlib.sha256("Grocery list\neggs\nmilk".encode()).hexdigest())

    def test_null_dates_fall_back_without_crashing(self):
        # Seen on a real database: a handful of notes carry NULL creation or
        # modification dates. Fall back to the other date, else the run time.
        uuid = add_note(self.con, 10, "Undated", "x")
        # Real databases populate ZCREATIONDATE3 and leave ZCREATIONDATE1 NULL.
        self.con.execute("UPDATE ZICCLOUDSYNCINGOBJECT SET ZCREATIONDATE1=NULL, ZCREATIONDATE3=? WHERE Z_PK=10", (apple(1700000000),))
        self.con.commit()
        self.run_sync()
        md = self.read("notes/notes/undated.md")
        self.assertIn("created: " + json.dumps(apple_time_to_iso(apple(1700000000))), md)
        # Losing every date is still not an edit: content identical ⇒ unchanged, no crash.
        self.con.execute("UPDATE ZICCLOUDSYNCINGOBJECT SET ZCREATIONDATE3=NULL, ZMODIFICATIONDATE1=NULL WHERE Z_PK=10")
        self.con.commit()
        result = self.run_sync()
        self.assertEqual(result["unchanged"], 1)
        self.assertEqual(self.index()[uuid]["created"], apple_time_to_iso(apple(1700000000)))
        # A brand-new note with no dates at all gets the run time for both.
        add_note(self.con, 11, "Also undated", "y")
        self.con.execute("UPDATE ZICCLOUDSYNCINGOBJECT SET ZCREATIONDATE1=NULL, ZMODIFICATIONDATE1=NULL WHERE Z_PK=11")
        self.con.commit()
        self.run_sync()
        entry = self.index()["00000000-0000-0000-0000-000000000011"]
        self.assertEqual(entry["created"], entry["modified"])

    def test_okf_indexes_written(self):
        add_note(self.con, 10, "One", "x")
        self.run_sync()
        self.assertIn("okf_version: '0.2'", self.read("index.md"))
        self.assertIn("One", self.read("notes/index.md"))
        self.assertIn("notes/notes/one.md", self.read("notes/index.md"))

    def test_attachments_become_lines_and_frontmatter(self):
        add_note(self.con, 10, "Pic", "before\n￼\nafter", attachments=[("ATT-1", "public.jpeg")])
        add_attachment(self.con, 10, "ATT-1", "public.jpeg", "IMG_0001.jpeg")
        self.run_sync()
        md = self.read("notes/notes/pic.md")
        self.assertIn("before\n[attachment: image/jpeg IMG_0001.jpeg]\nafter", md)
        self.assertIn('attachments:\n  - "image/jpeg IMG_0001.jpeg"', md)

    def test_unresolvable_attachment_keeps_placeholder_line(self):
        add_note(self.con, 10, "Pic", "￼", attachments=[("ATT-404", "public.jpeg")])
        self.run_sync()
        self.assertIn("[attachment: image/jpeg ATT-404]", self.read("notes/notes/pic.md"))

    # -- mutability --

    def test_edit_rewrites_same_path_and_updates_hash(self):
        uuid = add_note(self.con, 10, "Plan", "v1")
        self.run_sync()
        set_body(self.con, 10, "v2", modified=1782475400)
        result = self.run_sync()
        self.assertEqual(result["updated"], 1)
        self.assertTrue(self.read("notes/notes/plan.md").endswith("\nv2\n"))
        self.assertEqual(self.state_json()["notes"][uuid]["sha256"], hashlib.sha256(b"Plan\nv2").hexdigest())

    def test_modified_only_bump_writes_nothing(self):
        add_note(self.con, 10, "Plan", "v1")
        self.run_sync()
        path = self.out / "notes/notes/plan.md"
        before = (path.read_text(), path.stat().st_mtime_ns, self.state.read_text(), self.state.stat().st_mtime_ns)
        set_body(self.con, 10, "v1", modified=1782475400)
        result = self.run_sync()
        self.assertEqual(result["unchanged"], 1)
        after = (path.read_text(), path.stat().st_mtime_ns, self.state.read_text(), self.state.stat().st_mtime_ns)
        self.assertEqual(after, before)

    def test_noop_run_touches_nothing(self):
        add_note(self.con, 10, "Plan", "v1")
        self.run_sync()
        snapshot = {p: p.stat().st_mtime_ns for p in self.out.rglob("*") if p.is_file()}
        self.run_sync()
        self.assertEqual({p: p.stat().st_mtime_ns for p in self.out.rglob("*") if p.is_file()}, snapshot)

    def test_rename_moves_file_and_history_follows_in_git(self):
        subprocess.run(["git", "init", "-q", str(self.out)], check=True)
        uuid = add_note(self.con, 10, "Old title", "body")
        self.run_sync()
        subprocess.run(["git", "-C", str(self.out), "add", "-A"], check=True)
        subprocess.run(["git", "-C", str(self.out), "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "first"], check=True)
        self.con.execute("UPDATE ZICCLOUDSYNCINGOBJECT SET ZTITLE1='New title', ZMODIFICATIONDATE1=? WHERE Z_PK=10", (apple(1782475400),))
        self.con.commit()
        result = self.run_sync()
        self.assertEqual(result["renamed"], 1)
        self.assertFalse((self.out / "notes/notes/old-title.md").exists())
        self.assertTrue((self.out / "notes/notes/new-title.md").exists())
        self.assertEqual(self.index()[uuid]["path"], "notes/notes/new-title.md")
        subprocess.run(["git", "-C", str(self.out), "add", "-A"], check=True)
        subprocess.run(["git", "-C", str(self.out), "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "rename"], check=True)
        log = subprocess.run(["git", "-C", str(self.out), "log", "--follow", "--format=%s", "notes/notes/new-title.md"],
                             capture_output=True, text=True, check=True).stdout.split()
        self.assertEqual(log, ["rename", "first"])

    def test_folder_move(self):
        add_note(self.con, 10, "Memo", "body")
        self.run_sync()
        self.con.execute("UPDATE ZICCLOUDSYNCINGOBJECT SET ZFOLDER=?, ZMODIFICATIONDATE1=? WHERE Z_PK=10", (FOLDER_WORK, apple(1782475400)))
        self.con.commit()
        self.run_sync()
        self.assertFalse((self.out / "notes/notes/memo.md").exists())
        self.assertIn('folder: "Work"', self.read("notes/work/memo.md"))

    def test_deleted_note_removed_with_tombstone(self):
        uuid = add_note(self.con, 10, "Gone", "body")
        self.run_sync()
        self.con.execute("UPDATE ZICCLOUDSYNCINGOBJECT SET ZMARKEDFORDELETION=1 WHERE Z_PK=10")
        self.con.commit()
        result = self.run_sync()
        self.assertEqual(result["deleted"], 1)
        self.assertFalse((self.out / "notes/notes/gone.md").exists())
        entry = self.index()[uuid]
        self.assertIn("deleted", entry)
        self.assertNotIn("path", entry)
        self.assertNotIn(uuid, self.state_json()["notes"])

    def test_note_in_recently_deleted_folder_counts_as_deleted(self):
        uuid = add_note(self.con, 10, "Trashed", "body")
        self.run_sync()
        self.con.execute("UPDATE ZICCLOUDSYNCINGOBJECT SET ZFOLDER=? WHERE Z_PK=10", (FOLDER_TRASH,))
        self.con.commit()
        self.run_sync()
        self.assertFalse((self.out / "notes/notes/trashed.md").exists())
        self.assertIn("deleted", self.index()[uuid])

    def test_tombstone_pruned_after_thirty_days(self):
        uuid = add_note(self.con, 10, "Gone", "body")
        self.run_sync()
        self.con.execute("UPDATE ZICCLOUDSYNCINGOBJECT SET ZMARKEDFORDELETION=1 WHERE Z_PK=10")
        self.con.commit()
        self.run_sync()
        idx = self.index()
        idx[uuid]["deleted"] = "2020-01-01T00:00:00+00:00"
        (self.out / "notes/index.json").write_text(json.dumps(idx))
        self.run_sync()
        self.assertNotIn(uuid, self.index())

    def test_password_protected_note_ignored_entirely(self):
        uuid = add_note(self.con, 10, "Vault", "secret body", locked=1)
        result = self.run_sync()
        self.assertEqual(result["new"], 0)
        self.assertEqual(list(self.out.glob("notes/*/*.md")), [])
        self.assertNotIn(uuid, self.index())
        self.assertNotIn(uuid, self.state_json()["notes"])

    def test_slug_collision_gets_uuid_suffix(self):
        a = add_note(self.con, 10, "Same", "a", uuid="AAAAAAAA-0000-0000-0000-000000000010")
        b = add_note(self.con, 11, "Same", "b", uuid="BBBBBBBB-0000-0000-0000-000000000011")
        self.run_sync()
        paths = sorted(p.name for p in (self.out / "notes/notes").glob("*.md"))
        self.assertEqual(paths, ["same-bbbbbbbb.md", "same.md"])
        self.assertEqual(self.index()[a]["path"], "notes/notes/same.md")
        self.assertEqual(self.index()[b]["path"], "notes/notes/same-bbbbbbbb.md")

    def test_title_needing_yaml_quoting(self):
        add_note(self.con, 10, 'Weird: "title" #1', "body\n---\nnot frontmatter")
        self.run_sync()
        md = self.read("notes/notes/weird-title-1.md")
        self.assertIn('title: "Weird: \\"title\\" #1"', md)
        self.assertTrue(md.endswith("\nbody\n---\nnot frontmatter\n"))

    # -- scrubber integration (#1056) --

    def test_secret_in_body_redacted_with_frontmatter(self):
        # Real notes start with their title line; the secrets sit below it.
        add_note(self.con, 10, "Creds", "Creds\nkey AKIAIOSFODNN7EXAMPLE\n-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----")  # pii-ok gitleaks:allow synthetic fixture
        self.run_sync()
        md = self.read("notes/notes/creds.md")
        self.assertIn("[REDACTED:aws-access-key]", md)
        self.assertIn("[REDACTED:private-key]", md)
        self.assertNotIn("AKIAIOSFODNN7EXAMPLE", md)
        self.assertIn('redactions:\n  - "aws-access-key"\n  - "private-key"', md)
        self.assertEqual(self.index()[list(self.index())[0]]["redactions"], ["aws-access-key", "private-key"])

    def test_secret_in_title_quarantines_note(self):
        uuid = add_note(self.con, 10, "AKIAIOSFODNN7EXAMPLE", "body")
        result = self.run_sync()
        self.assertEqual(result["skipped"], 1)
        self.assertEqual(list(self.out.glob("notes/*/*.md")), [])
        self.assertNotIn(uuid, self.index())
        self.assertEqual(self.state_json()["skipped"][uuid]["reason"], "title-secret")

    def test_secret_on_first_line_quarantines_even_when_stored_title_is_truncated(self):
        # Apple stores a truncated first line as the title; a JWT cut mid-way no
        # longer matches the JWT pattern but its prefix would still leak into
        # the filename. The first body line is the real title, so scan that.
        header = "eyJhbGciOiJIUzI1NiJ9"
        payload = "eyJ1c2VySWQiOiJ1c2VyXzAxIn0"
        token = f"{header}.{payload}.c2lnbmF0dXJlc2lnbmF0dXJl"
        from scrub import scrub as _scrub
        title = token[:30]  # cut inside the payload: no third segment, no JWT match
        self.assertEqual(_scrub(title)[1], [], "fixture must not be caught by the title alone")
        uuid = add_note(self.con, 10, title, f"{token}\n\nmore text")
        result = self.run_sync()
        self.assertEqual(result["skipped"], 1)
        self.assertEqual(self.state_json()["skipped"][uuid]["reason"], "title-secret")
        self.assertEqual(list(self.out.glob("notes/*/*.md")), [])

    def test_first_line_quarantine_persists_across_unchanged_runs(self):
        # Regression: a note quarantined by the first-line scan was exported on
        # the next run because the cached skip entry short-circuited the scan
        # without carrying its reason forward.
        jwt = "eyJhbGciOiJIUzI1NiJ9.eyJ1c2VySWQiOiJ1c2VyXzAxIn0.c2lnbmF0dXJlc2lnbmF0dXJl"  # pii-ok gitleaks:allow synthetic fixture
        uuid = add_note(self.con, 10, jwt[:30], f"{jwt}\n\nmore text")
        self.run_sync()
        for _ in range(2):
            result = self.run_sync()
            self.assertEqual((result["skipped"], result["new"]), (1, 0))
            self.assertEqual(self.state_json()["skipped"][uuid]["reason"], "title-secret")
            self.assertEqual(list(self.out.glob("notes/*/*.md")), [])

    def test_secret_far_down_a_long_first_line_is_redacted_not_quarantined(self):
        # A 500 KB single-line HTML dump with a card number deep inside cannot
        # leak into the filename (the title is a short prefix), so it exports
        # with the body redacted instead of vanishing from the bundle.
        first = "<html lang=en>" + "x" * 5000 + " card 4111 1111 1111 1111 " + "y" * 5000
        uuid = add_note(self.con, 10, first[:60], first + "\nsecond line")
        result = self.run_sync()
        self.assertEqual((result["skipped"], result["new"]), (0, 1))
        md = self.read("notes/notes/html-lang-en-" + "x" * 46 + ".md")
        self.assertIn("[REDACTED:credit-card]", md)
        self.assertEqual(self.index()[uuid]["redactions"], ["credit-card"])

    def test_quarantine_re_evaluated_when_scrub_rules_change(self):
        import apple_notes_sync
        uuid = add_note(self.con, 10, "AKIAIOSFODNN7EXAMPLE", "AKIAIOSFODNN7EXAMPLE\nbody")  # pii-ok: synthetic fixture
        self.run_sync()
        self.assertEqual(self.state_json()["skipped"][uuid]["rules"], apple_notes_sync.SCRUB_RULES)
        # Simulate a scrubber release whose rules no longer flag this title.
        with unittest.mock.patch.object(apple_notes_sync, "SCRUB_RULES", "test-next"), \
             unittest.mock.patch.object(apple_notes_sync, "scrub", lambda text: (text, [])):
            result = self.run_sync()
        self.assertEqual((result["skipped"], result["new"]), (0, 1))
        self.assertNotIn(uuid, self.state_json()["skipped"])

    def test_skip_folders_and_skip_notes_config(self):
        a = add_note(self.con, 10, "Work thing", "x", folder=FOLDER_WORK)
        b = add_note(self.con, 11, "Personal", "y")
        c = add_note(self.con, 12, "Kept", "z")
        result = self.run_sync(skip_folders=["Work"], skip_notes=[b])
        self.assertEqual(result["skipped"], 2)
        self.assertEqual([p.name for p in self.out.glob("notes/*/*.md")], ["kept.md"])
        idx = self.index()
        self.assertNotIn(a, idx)
        self.assertNotIn(b, idx)
        self.assertIn(c, idx)
        skipped = self.state_json()["skipped"]
        self.assertEqual(skipped[a]["reason"], "skip-folder")
        self.assertEqual(skipped[b]["reason"], "skip-note")

    def test_quarantining_a_previously_written_note_removes_it(self):
        uuid = add_note(self.con, 10, "Was fine", "x")
        self.run_sync()
        self.assertTrue((self.out / "notes/notes/was-fine.md").exists())
        self.run_sync(skip_notes=[uuid])
        self.assertFalse((self.out / "notes/notes/was-fine.md").exists())
        self.assertNotIn(uuid, self.index())

    # -- safety --

    def test_database_opened_read_only(self):
        add_note(self.con, 10, "N", "x")
        before = self.db.read_bytes()
        self.run_sync()
        self.assertEqual(self.db.read_bytes(), before)

    def test_files_written_private(self):
        mask = os.umask(0o022)
        self.addCleanup(os.umask, mask)
        add_note(self.con, 10, "N", "x")
        self.run_sync()
        self.assertEqual((self.out / "notes/notes/n.md").stat().st_mode & 0o777, 0o600)
        self.assertEqual((self.out / "notes").stat().st_mode & 0o777, 0o700)


if __name__ == "__main__":
    unittest.main()
