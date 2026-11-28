"""Export iPhone call recordings from Apple Notes as meeting transcripts.

iOS 18.1+ saves a recorded phone call as an audio attachment in Notes
(folder "Call Recordings"). The transcript is not in the note body: it lives
in the attachment row's `ZICCLOUDSYNCINGOBJECT.ZMERGEABLEDATA1`, an
uncompressed Apple CRDT document (the same "mergeable data" object graph that
Notes uses for tables), shaped like this:

    document: 3=entry*, 4=key name*, 5=type name*, 6=uuid*
    entry:    1=register {2=value}  |  5=list {1=item {2=value}}
              | 13=map {1=type index, 3=field {1=key index, 2=value}}
    value:    1=int, 2=uint, 3=double, 4=string, 6=index of another entry

Resolved, the root entry is a `com.apple.notes.ICTTAudioRecording` map:

    callRecordingStartTime  NSDate  {doubleValue: unix seconds}
    callLocalSpeakerHandle  NSString {self: "+1..."}
    callRemoteSpeakerHandle NSString {self: "+1..."}
    summary / topLineSummary  NSString, when Apple Intelligence wrote one
    fragments               list of ICTTAudioRecording.Fragment
      Fragment.identity     the child audio attachment's ZIDENTIFIER
      Fragment.transcript   text CRDT whose runs point at
                            ICTTTranscriptSegment maps, one per word:
                            speaker {self}, text {self}, timestamp, duration
                            (seconds from the start of that fragment)

Segments are ordered by (fragment, timestamp), which matches the order the
transcript text CRDT gives them; reading the text CRDT is not needed.

The output follows FlyOnTheWall's `MeetingDoc::to_markdown()` contract, so
`augmentagent-channel-fotw` ingests a call exactly like a recorded meeting:
the summary and action items reach the wiki, the transcript stays in the
repository for the ask path to read.
"""
import json
import re
import struct
from datetime import datetime, timezone

from apple_notes_sync import _walk, scrub, slugify

RECORDING_UTI = "com.apple.m4a-audio"
ROOT_TYPE = "com.apple.notes.ICTTAudioRecording"
SEGMENT_TYPE = "com.apple.notes.ICTTTranscriptSegment"
# A speaker's turn ends after this much silence from them, or once the other
# side has talked for longer than INTERRUPT_S since their last word (a short
# "yeah" over someone does not split their sentence).
TURN_GAP_S = 2.0
INTERRUPT_S = 1.0


class NotACallRecording(ValueError):
    """The blob decoded, but it is an ordinary voice memo, not a phone call."""


# --- CRDT object graph -------------------------------------------------------

class _Graph:
    def __init__(self, blob):
        fields = {}
        for f, w, v in _walk(blob):
            if w == 2:
                fields.setdefault(f, []).append(v)
        if 3 not in fields or 4 not in fields or 5 not in fields:
            raise ValueError("not a mergeable data document")
        self.keys = [k.decode("utf-8", "replace") for k in fields[4]]
        self.types = [t.decode("utf-8", "replace") for t in fields[5]]
        self.entries = []
        for raw in fields[3]:
            parts = [(f, v) for f, w, v in _walk(raw) if w == 2]
            self.entries.append(parts[0] if parts else (0, b""))

    def value(self, raw, depth):
        for f, w, v in _walk(raw):
            if f == 6 and w == 0:
                return self.resolve(v, depth + 1)
            if f == 4 and w == 2:
                return v.decode("utf-8", "replace")
            if f == 2 and w == 0:
                return v
            if f == 3 and w == 1:
                return struct.unpack("<d", v)[0]
            if f == 1 and w == 0:
                return v
        return None

    def type_of(self, index):
        kind, body = self.entries[index]
        if kind != 13:
            return None
        for f, w, v in _walk(body):
            if f == 1 and w == 0:
                return self.types[v] if v < len(self.types) else None
        return None

    def resolve(self, index, depth=0):
        if depth > 64 or index >= len(self.entries):
            return None
        kind, body = self.entries[index]
        if kind == 13:  # typed map
            out = {"_type": self.type_of(index)}
            for f, w, v in _walk(body):
                if f == 3 and w == 2:
                    key = val = None
                    for ff, ww, vv in _walk(v):
                        if ff == 1 and ww == 0:
                            key = vv
                        elif ff == 2 and ww == 2:
                            val = vv
                    if key is not None and key < len(self.keys) and val is not None:
                        out[self.keys[key]] = self.value(val, depth)
            return out
        if kind == 1:  # last-writer-wins register
            for f, w, v in _walk(body):
                if f == 2 and w == 2:
                    return self.value(v, depth)
            return None
        if kind == 5:  # list
            items = []
            for f, w, v in _walk(body):
                if f == 1 and w == 2:
                    for ff, ww, vv in _walk(v):
                        if ff == 2 and ww == 2:
                            items.append(self.value(vv, depth))
            return items
        return None


def _scalar(node):
    """Unwrap NSString/NSNumber/NSDate maps to their plain value."""
    if isinstance(node, dict):
        for key in ("self", "doubleValue", "integerValue"):
            if key in node:
                return node[key]
        return None
    return node


def _segment_indices(graph, transcript_entry_index):
    """Entry indices of the segments one fragment's transcript text CRDT
    points at. The CRDT's attribute runs reference segments by entry index;
    collect every segment-typed index reachable from it."""
    found = []
    seen = set()

    def scan(buf, depth):
        if depth > 8:
            return
        try:
            fields = list(_walk(buf))
        except ValueError:
            return
        for f, w, v in fields:
            if f == 6 and w == 0 and v < len(graph.entries) and v not in seen:
                if graph.type_of(v) == SEGMENT_TYPE:
                    seen.add(v)
                    found.append(v)
            elif w == 2 and v:
                scan(v, depth + 1)

    kind, body = graph.entries[transcript_entry_index]
    scan(body, 0)
    return found


def _fragment_transcript_index(graph, fragment_index):
    kind, body = graph.entries[fragment_index]
    transcript_key = graph.keys.index("transcript") if "transcript" in graph.keys else None
    for f, w, v in _walk(body):
        if f == 3 and w == 2:
            key = target = None
            for ff, ww, vv in _walk(v):
                if ff == 1 and ww == 0:
                    key = vv
                elif ff == 2 and ww == 2:
                    for fff, www, vvv in _walk(vv):
                        if fff == 6 and www == 0:
                            target = vvv
            if key == transcript_key:
                return target
    return None


def _fragment_entry_indices(graph):
    """Fragment map indices, in the recording's list order."""
    root = graph.entries[0]
    if root[0] != 13 or graph.type_of(0) != ROOT_TYPE:
        return []
    fragments_key = graph.keys.index("fragments") if "fragments" in graph.keys else None
    list_index = None
    for f, w, v in _walk(root[1]):
        if f == 3 and w == 2:
            key = None
            for ff, ww, vv in _walk(v):
                if ff == 1 and ww == 0:
                    key = vv
                elif ff == 2 and ww == 2 and key == fragments_key:
                    for fff, www, vvv in _walk(vv):
                        if fff == 6 and www == 0:
                            list_index = vvv
    if list_index is None or graph.entries[list_index][0] != 5:
        return []
    out = []
    for f, w, v in _walk(graph.entries[list_index][1]):
        if f == 1 and w == 2:
            for ff, ww, vv in _walk(v):
                if ff == 2 and ww == 2:
                    for fff, www, vvv in _walk(vv):
                        if fff == 6 and www == 0:
                            out.append(vvv)
    return out


def decode_call_recording(blob, fragment_durations=None):
    """Decode `ZMERGEABLEDATA1` of a call-recording attachment.

    `fragment_durations` maps a fragment identity (the child audio
    attachment's ZIDENTIFIER) to its length in seconds, so a recording made
    of several fragments gets one continuous timeline.

    Returns {started_at, local_handle, remote_handle, summary, words,
    duration}, where
    words is [(seconds_from_start, duration, handle, text), ...] in order.
    Raises NotACallRecording for a voice memo and ValueError for anything
    that is not a recording document."""
    graph = _Graph(blob)
    if graph.type_of(0) != ROOT_TYPE:
        raise ValueError("root is not an audio recording")
    root = graph.resolve(0)
    remote = _scalar(root.get("callRemoteSpeakerHandle"))
    if not remote:
        raise NotACallRecording("audio recording without a call handle")
    durations = fragment_durations or {}
    words = []
    offset = 0.0
    audio_s = 0.0
    for fragment_index in _fragment_entry_indices(graph):
        fragment = graph.resolve(fragment_index) or {}
        transcript_index = _fragment_transcript_index(graph, fragment_index)
        frag_words = []
        if transcript_index is not None:
            for seg_index in _segment_indices(graph, transcript_index):
                seg = graph.resolve(seg_index) or {}
                text = _scalar(seg.get("text"))
                if not text:
                    continue
                start = float(_scalar(seg.get("timestamp")) or 0.0)
                length = float(_scalar(seg.get("duration")) or 0.0)
                frag_words.append((offset + start, length, _scalar(seg.get("speaker")) or "", text))
        frag_words.sort(key=lambda w: w[0])
        words.extend(frag_words)
        known = durations.get(_scalar(fragment.get("identity")) or "")
        if known:
            offset += float(known)
            audio_s += float(known)
        elif frag_words:
            offset = frag_words[-1][0] + frag_words[-1][1]
    started = _scalar(root.get("callRecordingStartTime"))
    summary = _scalar(root.get("summary")) or _scalar(root.get("topLineSummary")) or ""
    return {
        "started_at": float(started) if started else None,
        "local_handle": _scalar(root.get("callLocalSpeakerHandle")) or "",
        "remote_handle": remote,
        "summary": summary.strip() if isinstance(summary, str) else "",
        "words": words,
        # Length of the audio when Notes recorded it, else up to the last word.
        "duration": audio_s or (words[-1][0] + words[-1][1] if words else 0.0),
    }


# --- rendering ---------------------------------------------------------------

def speaker_turns(words, gap=TURN_GAP_S, interrupt=INTERRUPT_S):
    """Group word segments into [(start_seconds, speaker_handle, text)].

    Each speaker keeps an open turn that ends after a pause longer than `gap`
    or once others have spoken for more than `interrupt` seconds since that
    speaker's last word. Turns are then ordered by start time. Grouping on
    every speaker change instead would cut both sides into single words
    wherever people talk over each other."""
    open_turns = {}
    turns = []
    for start, length, handle, text in words:
        for other, turn in open_turns.items():
            if other != handle:
                turn[4] += length
        current = open_turns.get(handle)
        if current is not None and start - current[3] <= gap and current[4] <= interrupt:
            current[2].append(text)
            current[3] = start + length
            current[4] = 0.0
        else:
            current = [start, handle, [text], start + length, 0.0]
            open_turns[handle] = current
            turns.append(current)
    turns.sort(key=lambda t: t[0])
    return [(start, handle, "".join(parts).strip()) for start, handle, parts, _, _ in turns]


def _hms(seconds, rounded=False):
    """HH:MM:SS. Timestamps truncate (a word at 2.6s is at 00:00:02);
    durations round."""
    seconds = max(0, int(round(seconds)) if rounded else int(seconds))
    return f"{seconds // 3600:02d}:{seconds % 3600 // 60:02d}:{seconds % 60:02d}"


def remote_name(title, remote_handle):
    """iOS titles a recording "Call with <contact name or number>"."""
    match = re.match(r"^\s*Call with\s+(.+?)\s*$", title or "")
    return match.group(1) if match else (remote_handle or "Caller")


def meeting_id(recording_identifier):
    return "call-" + recording_identifier.lower()


def meeting_path(started_at, title, recording_identifier, tz=None):
    day = datetime.fromtimestamp(started_at, tz or timezone.utc).astimezone(tz).strftime("%Y-%m-%d")
    slug = slugify(title or "call") or "call"
    return f"meetings/{day}-{slug}-{recording_identifier.replace('-', '').lower()[:8]}.md"


def _yaml(value):
    return json.dumps(value, ensure_ascii=False)


def render_meeting(recording, title, recording_identifier, duration_s, summary="", action_items=(),
                   tz=None, tz_name=""):
    """Render one call in FlyOnTheWall's meeting markdown contract.
    Returns (markdown, redaction_kinds)."""
    started = recording["started_at"]
    local = recording["local_handle"]
    other = remote_name(title, recording["remote_handle"])
    lines = []
    for start, handle, text in speaker_turns(recording["words"]):
        who = "Me" if handle == local else other
        lines.append(f"[{_hms(start)}] {who}: {text}")
    transcript, findings = scrub("\n".join(lines))
    summary, summary_findings = scrub(summary or recording.get("summary") or "")
    action_items = [scrub(a)[0] for a in action_items]
    kinds = sorted({f.kind for f in findings} | {f.kind for f in summary_findings})
    when = datetime.fromtimestamp(started, timezone.utc).astimezone(tz)
    fm = [
        "---",
        "type: meeting-transcript",
        f"id: {_yaml(meeting_id(recording_identifier))}",
        f"title: {_yaml(title)}",
        f"date: {_yaml(when.strftime('%Y-%m-%d'))}",
        f"started_at_ms: {int(started * 1000)}",
        f"duration: {_yaml(_hms(duration_s, rounded=True))}",
        f"timezone: {_yaml(tz_name)}",
        "attendees:",
        f"  - {_yaml(other)}",
        "tags:",
        '  - "phone-call"',
        # iOS plays a "this call is being recorded" notice to every party.
        "disclosed: true",
        'source: "apple-notes-call-recording"',
        f"call_remote_handle: {_yaml(recording['remote_handle'])}",
    ]
    if kinds:
        fm.append("redactions:")
        fm.extend(f"  - {_yaml(k)}" for k in kinds)
    fm.append("---")
    body = [f"# {title}", ""]
    if summary.strip():
        body += [summary.strip(), ""]
    if action_items:
        body += ["## Action items", ""] + [f"- [ ] {a}" for a in action_items] + [""]
    body += ["## Transcript", "", transcript, ""]
    return "\n".join(fm) + "\n\n" + "\n".join(body), kinds


# --- export ------------------------------------------------------------------

# Notes rewrites a recording while the transcript is still arriving from the
# phone. Jarvis keys a meeting by id and ingests it once, so wait for the
# note to go quiet before the first export.
SETTLE_S = 5 * 60
# A summarizer that keeps failing must not hold a call back forever.
MAX_SUMMARY_ATTEMPTS = 3


def _recording_rows(con):
    return con.execute(
        """
        SELECT a.Z_PK, a.ZIDENTIFIER, a.ZTITLE, a.ZMERGEABLEDATA1,
               COALESCE(n.ZMODIFICATIONDATE1, n.ZMODIFICATIONDATE, a.ZMODIFICATIONDATE),
               n.ZISPASSWORDPROTECTED, n.ZMARKEDFORDELETION, a.ZMARKEDFORDELETION, f.ZFOLDERTYPE
        FROM ZICCLOUDSYNCINGOBJECT a
        JOIN ZICCLOUDSYNCINGOBJECT n ON n.Z_PK = a.ZNOTE
        LEFT JOIN ZICCLOUDSYNCINGOBJECT f ON f.Z_PK = n.ZFOLDER
        WHERE a.ZTYPEUTI = ? AND a.ZMERGEABLEDATA1 IS NOT NULL AND a.ZIDENTIFIER IS NOT NULL
        ORDER BY a.ZIDENTIFIER
        """,
        (RECORDING_UTI,),
    ).fetchall()


def _fragment_durations(con, recording_pk):
    rows = con.execute(
        "SELECT ZIDENTIFIER, ZDURATION FROM ZICCLOUDSYNCINGOBJECT WHERE ZPARENTATTACHMENT = ?",
        (recording_pk,),
    ).fetchall()
    return {ident: dur for ident, dur in rows if ident and dur}


def export_calls(db_path, out_dir, state_path, tz=None, tz_name="", summarizer=None, now=None,
                 settle_s=SETTLE_S, log=print):
    """Write every settled call recording in the Notes database to
    `out_dir/meetings/` as a meeting transcript. Never deletes or edits a
    file it did not write. Returns (counts, written_relative_paths).

    `summarizer(transcript_markdown) -> (summary, [action item, ...])` is
    called once per new or changed transcript; it may raise to defer."""
    import hashlib
    import sqlite3
    from pathlib import Path

    from apple_notes_sync import APPLE_EPOCH, FOLDER_TYPE_TRASH, _load_json, _write_private

    out_dir = Path(out_dir)
    state = _load_json(state_path, {"calls": {}})
    state.setdefault("calls", {})
    now = now if now is not None else datetime.now(timezone.utc).timestamp()
    counts = {k: 0 for k in ("new", "updated", "unchanged", "pending", "skipped")}
    written = []
    con = sqlite3.connect(f"file:{Path(db_path).resolve()}?mode=ro", uri=True)
    try:
        for pk, ident, title, blob, modified, locked, note_gone, att_gone, ftype in _recording_rows(con):
            if locked or note_gone or att_gone or ftype == FOLDER_TYPE_TRASH:
                continue
            try:
                recording = decode_call_recording(bytes(blob), _fragment_durations(con, pk))
            except NotACallRecording:
                continue
            except ValueError as error:
                counts["skipped"] += 1
                log(f"skipped recording {ident}: undecodable ({error})")
                continue
            if not recording["words"] or recording["started_at"] is None:
                counts["pending"] += 1
                continue
            if modified is not None and now - (modified + APPLE_EPOCH) < settle_s:
                counts["pending"] += 1
                continue
            title = (title or "").strip() or f"Call with {recording['remote_handle']}"
            words = recording["words"]
            duration = max(recording["duration"], max(w[0] + w[1] for w in words))
            fingerprint = hashlib.sha256(json.dumps([title, words, recording["summary"]]).encode()).hexdigest()
            entry = state["calls"].get(ident, {})
            rel = entry.get("path") or meeting_path(recording["started_at"], title, ident, tz)
            if entry.get("sha256") == fingerprint and (out_dir / rel).exists():
                counts["unchanged"] += 1
                continue
            summary, actions = recording["summary"], []
            if summarizer is not None and not summary:
                draft, _ = render_meeting(recording, title, ident, duration, tz=tz, tz_name=tz_name)
                try:
                    summary, actions = summarizer(draft)
                except Exception as error:  # noqa: BLE001 - any failure defers the call
                    attempts = entry.get("summary_attempts", 0) + 1
                    state["calls"][ident] = dict(entry, summary_attempts=attempts)
                    if attempts < MAX_SUMMARY_ATTEMPTS:
                        counts["pending"] += 1
                        log(f"summary failed for {ident} (attempt {attempts}); retrying next run: {error}")
                        continue
                    log(f"summary failed for {ident} {attempts} times; exporting without one")
                    summary, actions = "", []
            text, _ = render_meeting(recording, title, ident, duration, summary=summary,
                                     action_items=actions, tz=tz, tz_name=tz_name)
            _write_private(out_dir / rel, text)
            counts["updated" if entry.get("sha256") else "new"] += 1
            state["calls"][ident] = {"path": rel, "sha256": fingerprint}
            written.append(rel)
    finally:
        con.close()
    _write_private(state_path, json.dumps(state, indent=1, sort_keys=True) + "\n")
    return counts, written
