"""Call-recording export tests: CRDT decoding, speaker turns, the meeting
markdown contract, the export loop and the git flow. Every blob is built
here by a small encoder that mirrors what iOS writes; no real recording,
name or phone number is used."""
import contextlib
import io
import json
import os
import random
import sqlite3
import struct
import subprocess
import sys
import tempfile
import unittest
from datetime import timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import call_recordings as cr  # noqa: E402
import calls as cli  # noqa: E402

APPLE_EPOCH = 978307200
ME = "+15555550100"
THEM = "+15555550199"
START = 1782475200.5  # 2026-06-26 12:00:00.5 UTC
GIT_ID = ["-c", "user.email=t@example.com", "-c", "user.name=t", "-c", "commit.gpgsign=false"]


# --- encoder -----------------------------------------------------------------

def _vi(n):
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        out.append(b | (0x80 if n else 0))
        if not n:
            return bytes(out)


def _len(field, payload):
    return _vi(field << 3 | 2) + _vi(len(payload)) + payload


def _var(field, n):
    return _vi(field << 3) + _vi(n)


def _dbl(field, x):
    return _vi(field << 3 | 1) + struct.pack("<d", x)


def ref(index):
    return _var(6, index)


def s(text):
    return _len(4, text.encode())


def dbl(x):
    return _dbl(3, x)


def num(n):
    return _var(1, n)


class Doc:
    """Builds a mergeable data document. Entry 0 is reserved for the root."""

    def __init__(self):
        self.keys, self.types, self.entries = [], [], [b""]

    def _k(self, name):
        if name not in self.keys:
            self.keys.append(name)
        return self.keys.index(name)

    def _t(self, name):
        if name not in self.types:
            self.types.append(name)
        return self.types.index(name)

    def _add(self, entry, at=None):
        if at is not None:
            self.entries[at] = entry
            return at
        self.entries.append(entry)
        return len(self.entries) - 1

    def map(self, type_name, fields, at=None):
        body = _var(1, self._t(type_name))
        for key, value in fields.items():
            body += _len(3, _var(1, self._k(key)) + _len(2, value))
        return self._add(_len(13, body), at)

    def register(self, value):
        clock = _len(1, _var(1, 3) + _var(2, 1))
        return self._add(_len(1, clock + _len(2, value)))

    def string(self, text):
        return self.register(ref(self.map("com.apple.CRDT.NSString", {"self": s(text)})))

    def double(self, x):
        return self.register(ref(self.map("com.apple.CRDT.NSNumber", {"doubleValue": dbl(x)})))

    def list(self, values):
        items = b"".join(_len(1, _len(2, v)) for v in values)
        return self._add(_len(5, items))

    def text_crdt(self, targets):
        runs = b"".join(_len(5, _len(1, ref(t))) for t in targets)
        return self._add(_len(15, _len(1, _len(2, ("￼" * len(targets)).encode()) + runs)))

    def bytes(self):
        out = b"".join(_len(3, e) for e in self.entries)
        out += b"".join(_len(4, k.encode()) for k in self.keys)
        out += b"".join(_len(5, t.encode()) for t in self.types)
        return _len(1, b"") + out


def recording_blob(fragments, remote=THEM, local=ME, summary=None, start=START, shuffle_seed=7):
    """fragments: [(identity, [(t, dur, handle, text), ...]), ...]."""
    doc = Doc()
    frag_refs = []
    for identity, words in fragments:
        order = list(words)
        random.Random(shuffle_seed).shuffle(order)  # storage order is not spoken order
        seg_ids = []
        for t, dur, handle, text in order:
            seg_ids.append(doc.map(cr.SEGMENT_TYPE, {
                "identity": s(f"SEG-{len(seg_ids)}"),
                "speaker": ref(doc.string(handle)),
                "text": ref(doc.string(text)),
                "timestamp": ref(doc.double(t)),
                "duration": ref(doc.double(dur)),
            }))
        transcript = doc.text_crdt(seg_ids)
        frag_refs.append(ref(doc.map(cr.ROOT_TYPE + ".Fragment", {
            "identity": s(identity),
            "transcriptVersion": ref(doc.register(ref(doc.map("com.apple.CRDT.NSNumber", {"integerValue": num(2)})))),
            "transcript": ref(transcript),
        })))
    root = {
        "identity": s("00000000-0000-0000-0000-000000000000"),
        "callRecordingStartTime": ref(doc.map("com.apple.CRDT.NSDate", {"doubleValue": dbl(start)})),
        "fragments": ref(doc.list(frag_refs)),
        "callLocalSpeakerHandle": ref(doc.map("com.apple.CRDT.NSString", {"self": s(local)})),
        "callRecording": ref(doc.map("com.apple.CRDT.NSNumber", {"integerValue": num(2)})),
    }
    if remote:
        root["callRemoteSpeakerHandle"] = ref(doc.map("com.apple.CRDT.NSString", {"self": s(remote)}))
    if summary:
        root["summary"] = ref(doc.string(summary))
    doc.map(cr.ROOT_TYPE, root, at=0)
    return doc.bytes()


WORDS = [
    (0.5, 0.3, ME, "Hi"), (0.8, 0.3, ME, " there,"), (1.1, 0.4, ME, " thanks"), (1.5, 0.3, ME, " for"),
    (1.8, 0.4, ME, " calling."),
    (2.6, 0.4, THEM, "Happy"), (3.0, 0.2, THEM, " to."), (3.4, 0.3, THEM, " Shall"), (3.7, 0.2, THEM, " we"),
    (3.9, 0.4, THEM, " ship"), (4.3, 0.4, THEM, " Friday?"),
    (5.0, 0.3, ME, " Yes."),
]


# --- decoding ----------------------------------------------------------------

class DecodeTests(unittest.TestCase):
    def test_words_come_back_in_spoken_order_with_speakers(self):
        rec = cr.decode_call_recording(recording_blob([("FRAG-1", WORDS)]), {"FRAG-1": 6.0})
        self.assertEqual([w[3] for w in rec["words"]], [w[3] for w in WORDS])
        self.assertEqual([w[2] for w in rec["words"]], [w[2] for w in WORDS])
        self.assertEqual(rec["local_handle"], ME)
        self.assertEqual(rec["remote_handle"], THEM)
        self.assertAlmostEqual(rec["started_at"], START)
        self.assertEqual(rec["duration"], 6.0)
        self.assertEqual(rec["summary"], "")

    def test_apple_summary_is_carried(self):
        rec = cr.decode_call_recording(recording_blob([("F", WORDS)], summary=" Agreed to ship Friday. "))
        self.assertEqual(rec["summary"], "Agreed to ship Friday.")

    def test_later_fragments_continue_the_timeline(self):
        blob = recording_blob([("F1", [(1.0, 0.5, ME, "one")]), ("F2", [(0.5, 0.5, THEM, " two")])])
        rec = cr.decode_call_recording(blob, {"F1": 10.0, "F2": 3.0})
        self.assertEqual([(w[0], w[3]) for w in rec["words"]], [(1.0, "one"), (10.5, " two")])
        self.assertEqual(rec["duration"], 13.0)

    def test_duration_falls_back_to_the_last_word(self):
        rec = cr.decode_call_recording(recording_blob([("F", WORDS)]))
        self.assertAlmostEqual(rec["duration"], 5.3)

    def test_voice_memo_is_not_a_call(self):
        with self.assertRaises(cr.NotACallRecording):
            cr.decode_call_recording(recording_blob([("F", WORDS)], remote=None))

    def test_other_documents_are_rejected(self):
        doc = Doc()
        doc.map("com.apple.notes.ICTable", {"x": s("y")}, at=0)
        with self.assertRaises(ValueError):
            cr.decode_call_recording(doc.bytes())
        for junk in (b"", b"\x0a\x05abc", b"\xff\xff\xff"):
            with self.assertRaises(ValueError):
                cr.decode_call_recording(junk)


# --- turns and rendering -----------------------------------------------------

class TurnTests(unittest.TestCase):
    def test_turns_follow_speaker_changes(self):
        turns = cr.speaker_turns(WORDS)
        self.assertEqual(turns, [
            (0.5, ME, "Hi there, thanks for calling."),
            (2.6, THEM, "Happy to. Shall we ship Friday?"),
            (5.0, ME, "Yes."),
        ])

    def test_short_backchannel_does_not_split_a_sentence(self):
        words = [(0.0, 0.4, ME, "We"), (0.4, 0.4, ME, " can"), (0.6, 0.3, THEM, "Yeah."),
                 (0.8, 0.4, ME, " ship"), (1.2, 0.4, ME, " it.")]
        self.assertEqual(cr.speaker_turns(words), [(0.0, ME, "We can ship it."), (0.6, THEM, "Yeah.")])

    def test_long_pause_starts_a_new_turn(self):
        words = [(0.0, 0.5, ME, "First."), (5.0, 0.5, ME, " Second.")]
        self.assertEqual(cr.speaker_turns(words), [(0.0, ME, "First."), (5.0, ME, "Second.")])


class RenderTests(unittest.TestCase):
    def setUp(self):
        self.rec = cr.decode_call_recording(recording_blob([("F", WORDS)]), {"F": 6.0})

    def render(self, **kw):
        return cr.render_meeting(self.rec, "Call with Alex Example", "ABCDEF12-0000-0000-0000-000000000001",
                                 6.0, tz=timezone.utc, tz_name="UTC", **kw)

    def test_frontmatter_matches_the_fotw_meeting_contract(self):
        md, kinds = self.render()
        fm = md.split("---\n")[1]
        for line in ('type: meeting-transcript', 'id: "call-abcdef12-0000-0000-0000-000000000001"',
                     'title: "Call with Alex Example"', 'date: "2026-06-26"',
                     f"started_at_ms: {int(START * 1000)}", 'duration: "00:00:06"', 'timezone: "UTC"',
                     'attendees:\n  - "Alex Example"', 'tags:\n  - "phone-call"', "disclosed: true",
                     f'call_remote_handle: "{THEM}"'):
            self.assertIn(line, fm)
        self.assertEqual(kinds, [])

    def test_body_is_title_summary_actions_then_transcript(self):
        md, _ = self.render(summary="We agreed to ship Friday.", action_items=["Alex sends the build"])
        body = md.split("---\n", 2)[2]
        self.assertEqual(body.index("# Call with Alex Example") < body.index("We agreed") < body.index("## Action items")
                         < body.index("## Transcript"), True)
        self.assertIn("- [ ] Alex sends the build", body)
        self.assertIn("[00:00:00] Me: Hi there, thanks for calling.\n[00:00:02] Alex Example: Happy to.", body)

    def test_number_only_title_names_the_caller_by_handle(self):
        self.assertEqual(cr.remote_name(f"Call with {THEM}", THEM), THEM)
        self.assertEqual(cr.remote_name("Odd title", THEM), THEM)

    def test_secrets_said_on_the_call_are_scrubbed(self):
        rec = dict(self.rec, words=self.rec["words"] + [(5.5, 0.3, THEM, " password = hunter2hunter2")])
        md, kinds = cr.render_meeting(rec, "Call with Alex Example", "ABCDEF12-0000-0000-0000-000000000001",
                                      6.0, tz=timezone.utc, tz_name="UTC")
        self.assertNotIn("hunter2hunter2", md)
        self.assertIn("password-assignment", kinds)
        self.assertIn('redactions:\n  - "password-assignment"', md)

    def test_path_follows_the_fotw_filename_convention(self):
        self.assertEqual(cr.meeting_path(START, "Call with Alex Example", "ABCDEF12-0000-0000-0000-000000000001",
                                         timezone.utc),
                         "meetings/2026-06-26-call-with-alex-example-abcdef12.md")


# --- export loop -------------------------------------------------------------

def make_db(path):
    con = sqlite3.connect(path)
    con.executescript(
        """
        CREATE TABLE ZICCLOUDSYNCINGOBJECT (
            Z_PK INTEGER PRIMARY KEY, ZIDENTIFIER TEXT, ZTITLE TEXT, ZTITLE1 TEXT, ZTITLE2 TEXT,
            ZTYPEUTI TEXT, ZNOTE INTEGER, ZFOLDER INTEGER, ZFOLDERTYPE INTEGER, ZPARENTATTACHMENT INTEGER,
            ZDURATION REAL, ZMERGEABLEDATA1 BLOB, ZISPASSWORDPROTECTED INTEGER, ZMARKEDFORDELETION INTEGER,
            ZMODIFICATIONDATE REAL, ZMODIFICATIONDATE1 REAL
        );
        INSERT INTO ZICCLOUDSYNCINGOBJECT (Z_PK, ZTITLE2, ZFOLDERTYPE) VALUES (1, 'Call Recordings', 0);
        INSERT INTO ZICCLOUDSYNCINGOBJECT (Z_PK, ZTITLE2, ZFOLDERTYPE) VALUES (2, 'Recently Deleted', 1);
        """
    )
    con.commit()
    return con


def add_call(con, pk, ident, title, blob, *, folder=1, modified=START, locked=0, deleted=0, child_dur=6.0):
    note_pk, child_pk = pk + 1000, pk + 2000
    con.execute("INSERT INTO ZICCLOUDSYNCINGOBJECT (Z_PK, ZTITLE1, ZFOLDER, ZISPASSWORDPROTECTED, "
                "ZMARKEDFORDELETION, ZMODIFICATIONDATE1) VALUES (?, ?, ?, ?, ?, ?)",
                (note_pk, title, folder, locked, deleted, modified - APPLE_EPOCH))
    con.execute("INSERT INTO ZICCLOUDSYNCINGOBJECT (Z_PK, ZIDENTIFIER, ZTITLE, ZTYPEUTI, ZNOTE, ZMERGEABLEDATA1) "
                "VALUES (?, ?, ?, ?, ?, ?)", (pk, ident, title, cr.RECORDING_UTI, note_pk, blob))
    con.execute("INSERT INTO ZICCLOUDSYNCINGOBJECT (Z_PK, ZIDENTIFIER, ZTYPEUTI, ZPARENTATTACHMENT, ZDURATION) "
                "VALUES (?, 'F', 'public.mpeg-4-audio', ?, ?)", (child_pk, pk, child_dur))
    con.commit()


class ExportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="call export ")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.db = self.root / "NoteStore.sqlite"
        self.con = make_db(self.db)
        self.addCleanup(self.con.close)
        self.out = self.root / "transcripts"
        self.state = self.root / "state.json"
        self.logs = []

    def export(self, now=START + 3600, **kw):
        return cr.export_calls(self.db, self.out, self.state, tz=timezone.utc, tz_name="UTC", now=now,
                               log=self.logs.append, **kw)

    def test_exports_settled_calls_and_skips_the_rest(self):
        blob = recording_blob([("F", WORDS)])
        add_call(self.con, 10, "AAAAAAAA-0000-0000-0000-000000000001", "Call with Alex Example", blob)
        add_call(self.con, 11, "BBBBBBBB-0000-0000-0000-000000000002", "Call with Fresh", blob, modified=START + 3500)
        add_call(self.con, 12, "CCCCCCCC-0000-0000-0000-000000000003", "Call with Trash", blob, folder=2)
        add_call(self.con, 13, "DDDDDDDD-0000-0000-0000-000000000004", "Call with Locked", blob, locked=1)
        add_call(self.con, 14, "EEEEEEEE-0000-0000-0000-000000000005", "New Recording",
                 recording_blob([("F", WORDS)], remote=None))
        add_call(self.con, 15, "FFFFFFFF-0000-0000-0000-000000000006", "Call with Gone", blob, deleted=1)
        original = self.db.read_bytes()
        counts, written = self.export()
        self.assertEqual(written, ["meetings/2026-06-26-call-with-alex-example-aaaaaaaa.md"])
        self.assertEqual((counts["new"], counts["pending"]), (1, 1))
        md = (self.out / written[0]).read_text()
        self.assertIn('duration: "00:00:06"', md)
        self.assertEqual((self.out / written[0]).stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.db.read_bytes(), original)

    def test_rerun_is_a_no_op_and_a_changed_transcript_rewrites(self):
        ident = "AAAAAAAA-0000-0000-0000-000000000001"
        add_call(self.con, 10, ident, "Call with Alex Example", recording_blob([("F", WORDS)]))
        self.export()
        counts, written = self.export()
        self.assertEqual((counts["unchanged"], written), (1, []))
        self.con.execute("UPDATE ZICCLOUDSYNCINGOBJECT SET ZMERGEABLEDATA1 = ? WHERE Z_PK = 10",
                         (recording_blob([("F", WORDS + [(5.5, 0.3, THEM, " Great.")])]),))
        self.con.commit()
        counts, written = self.export()
        self.assertEqual((counts["updated"], len(written)), (1, 1))
        self.assertIn("Great.", (self.out / written[0]).read_text())

    def test_summarizer_runs_once_per_transcript(self):
        add_call(self.con, 10, "AAAAAAAA-0000-0000-0000-000000000001", "Call with Alex Example",
                 recording_blob([("F", WORDS)]))
        seen = []

        def summarizer(draft):
            seen.append(draft)
            return "Agreed to ship Friday.", ["Alex ships the build"]

        _, written = self.export(summarizer=summarizer)
        self.export(summarizer=summarizer)
        self.assertEqual(len(seen), 1)
        self.assertIn("## Transcript", seen[0])
        md = (self.out / written[0]).read_text()
        self.assertIn("Agreed to ship Friday.\n\n## Action items\n\n- [ ] Alex ships the build", md)

    def test_apple_summary_wins_over_the_summarizer(self):
        add_call(self.con, 10, "AAAAAAAA-0000-0000-0000-000000000001", "Call with Alex Example",
                 recording_blob([("F", WORDS)], summary="Apple wrote this."))
        _, written = self.export(summarizer=lambda d: self.fail("summarizer should not run"))
        self.assertIn("Apple wrote this.", (self.out / written[0]).read_text())

    def test_failing_summarizer_defers_then_gives_up(self):
        add_call(self.con, 10, "AAAAAAAA-0000-0000-0000-000000000001", "Call with Alex Example",
                 recording_blob([("F", WORDS)]))

        def broken(draft):
            raise RuntimeError("offline")

        for _ in range(cr.MAX_SUMMARY_ATTEMPTS - 1):
            counts, written = self.export(summarizer=broken)
            self.assertEqual((counts["pending"], written), (1, []))
        counts, written = self.export(summarizer=broken)
        self.assertEqual(counts["new"], 1)
        self.assertTrue(any("exporting without one" in line for line in self.logs))

    def test_undecodable_blob_is_reported_not_fatal(self):
        add_call(self.con, 10, "AAAAAAAA-0000-0000-0000-000000000001", "Call with Broken", b"\x0a\x05abc")
        counts, written = self.export()
        self.assertEqual((counts["skipped"], written), (1, []))


# --- CLI and git -------------------------------------------------------------

def git(repo, *args):
    return subprocess.run(["git", "-C", str(repo), *GIT_ID, *args], check=True, capture_output=True,
                          text=True).stdout


class CliTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="call cli ")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.db = self.root / "NoteStore.sqlite"
        con = make_db(self.db)
        add_call(con, 10, "AAAAAAAA-0000-0000-0000-000000000001", "Call with Alex Example",
                 recording_blob([("F", WORDS)]), modified=START)
        con.close()
        self.remote = self.root / "remote.git"
        subprocess.run(["git", "init", "-q", "--bare", "-b", "main", str(self.remote)], check=True)
        self.out = self.root / "clone"
        subprocess.run(["git", "clone", "-q", str(self.remote), str(self.out)], check=True, capture_output=True)
        (self.out / "meetings").mkdir()
        (self.out / "meetings" / "2026-06-01-fotw-meeting-01a00000.md").write_text("fotw owns this\n")
        git(self.out, "add", "-A")
        git(self.out, "commit", "-q", "-m", "fotw")
        git(self.out, "push", "-q", "origin", "HEAD")
        env = {"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.com", "GIT_COMMITTER_NAME": "t",
               "GIT_COMMITTER_EMAIL": "t@example.com", "GIT_CONFIG_GLOBAL": "/dev/null"}
        saved = {key: os.environ.get(key) for key in env}
        os.environ.update(env)
        self.addCleanup(lambda: [os.environ.pop(k, None) if v is None else os.environ.__setitem__(k, v)
                                 for k, v in saved.items()])
        mask = os.umask(0o022)
        self.addCleanup(os.umask, mask)

    def main(self, *extra):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = cli.main(["--db", str(self.db), "--out", str(self.out), "--state", str(self.root / "s.json"),
                             *extra])
        return code, out.getvalue(), err.getvalue()

    def test_commit_adds_only_exported_files_and_pushes(self):
        (self.out / "meetings" / "untracked-fotw-draft.md").write_text("not ours\n")
        code, out, err = self.main("--commit")
        self.assertEqual(code, 0, err)
        self.assertIn("committed and pushed", out)
        pushed = git(self.remote, "show", "--name-only", "--format=%s", "main")
        self.assertIn("Add 1 call transcript", pushed)
        self.assertIn("meetings/2026-06-26-call-with-alex-example-aaaaaaaa.md", pushed)
        self.assertNotIn("untracked-fotw-draft.md", pushed)
        self.assertIn("untracked-fotw-draft.md", git(self.out, "status", "--porcelain"))

    def test_remote_changes_are_merged_not_rebased(self):
        other = self.root / "other"
        subprocess.run(["git", "clone", "-q", str(self.remote), str(other)], check=True, capture_output=True)
        (other / "meetings" / "2026-06-27-new-fotw-01a00001.md").write_text("pushed meanwhile\n")
        git(other, "add", "-A")
        git(other, "commit", "-q", "-m", "fotw meanwhile")
        git(other, "push", "-q", "origin", "HEAD")
        code, out, err = self.main("--commit")
        self.assertEqual(code, 0, err)
        log = git(self.remote, "log", "--format=%s", "main")
        self.assertIn("fotw meanwhile", log)
        self.assertIn("Add 1 call transcript", log)

    def test_out_inside_the_source_checkout_is_rejected(self):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            cli.parser().parse_args(["--out", str(Path(__file__).resolve().parent)])

    def test_summarize_without_claude_fails_cleanly(self):
        old = os.environ.get("PATH", "")
        os.environ["PATH"] = str(self.root)
        self.addCleanup(os.environ.__setitem__, "PATH", old)
        code, _, err = self.main("--summarize")
        self.assertEqual(code, 1)
        self.assertIn("needs the `claude` CLI", err)


if __name__ == "__main__":
    unittest.main()
