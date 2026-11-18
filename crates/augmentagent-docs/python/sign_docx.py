"""Private stdin/stdout worker for Word (.docx) signing; never accepts filesystem paths or network URLs.

The signed file is the input file with one inline picture (and optional date)
added at a chosen paragraph. Every other zip member is copied byte-for-byte,
and document.xml outside the chosen paragraph is unchanged; both are verified
before any bytes are returned.
"""
import base64
import io
import json
import re
import sys
import zipfile
from xml.etree import ElementTree as ET

from PIL import Image, ImageDraw, ImageFont

MAX_DOCX = 8 * 1024 * 1024
MAX_PART = 32 * 1024 * 1024
DOC = "word/document.xml"
RELS = "word/_rels/document.xml.rels"
TYPES = "[Content_Types].xml"
EMU_PER_PT = 12700
DEFAULT_WIDTH_PT = 144
FIXED_TIME = (1980, 1, 1, 0, 0, 0)
BLOCKED = re.compile(r"\b(affidavit|sworn\s+statement|penalty\s+of\s+perjury|government\s+filing)\b", re.I)
SIGN_WORD = re.compile(r"\bsign(?:ature|ed|\s+here)?\b", re.I)
UNDERSCORES = re.compile(r"_{3,}")
# <w:p>, <w:p attr="…">, and their self-closing forms <w:p/> and <w:p attr="…"/>.
P_OPEN = re.compile(r"<w:p(?:\s[^>]*)?/?>")
P_CLOSE = "</w:p>"
T_TEXT = re.compile(r"<w:t(?:\s[^>]*)?>([^<]*)</w:t>")
R_SPAN = re.compile(r"<w:r(?:\s[^>]*)?>.*?</w:r>", re.S)
RPR = re.compile(r"<w:rPr>.*?</w:rPr>|<w:rPr/>", re.S)


XML_REF = re.compile(r"&(#x[0-9a-fA-F]+|#[0-9]+|lt|gt|amp|quot|apos);")
NAMED = {"lt": "<", "gt": ">", "amp": "&", "quot": '"', "apos": "'"}


def unescape(text):
    """XML character data to text, in one pass: numeric references (`&#65;`,
    `&#x41;`) and the five predefined entities. A single pass never reads a
    decoded character as markup again (`&#38;lt;` is the text `&lt;`)."""
    def ref(m):
        name = m.group(1)
        if name.startswith("#x"):
            return chr(int(name[2:], 16))
        if name.startswith("#"):
            return chr(int(name[1:]))
        return NAMED[name]
    return XML_REF.sub(ref, text)


def escape(text):
    return text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def load(data):
    raw = base64.b64decode(data["docx"], validate=True)
    if len(raw) > MAX_DOCX:
        raise ValueError("document exceeds size limit")
    try:
        z = zipfile.ZipFile(io.BytesIO(raw))
    except zipfile.BadZipFile:
        raise ValueError("not a Word .docx: the file is not a zip package")
    names = z.namelist()
    if DOC not in names or TYPES not in names:
        raise ValueError("not a Word .docx: no word/document.xml inside")
    if any(i.file_size > MAX_PART for i in z.infolist()) or len(names) > 5000:
        raise ValueError("document parts exceed size limit")
    types = z.read(TYPES).decode("utf-8")
    if any(n.lower().endswith("vbaproject.bin") for n in names) or "macroEnabled" in types:
        raise ValueError("macro-enabled Word documents require manual signing")
    doc = z.read(DOC).decode("utf-8")
    try:
        ET.fromstring(doc.encode("utf-8"))
    except ET.ParseError:
        raise ValueError("not a readable .docx: word/document.xml is not valid XML")
    return raw, z, doc


def paragraphs(doc):
    """Every <w:p> element as (start, end, text), end just past </w:p> (or
    past a self-closing <w:p/>, an empty paragraph as Word often writes it in
    an empty table cell). Paragraphs nest (text boxes), so open/close tags are
    paired with a stack."""
    events = []
    for m in P_OPEN.finditer(doc):
        if m.group(0).endswith("/>"):
            events.append((m.start(), "empty", m.end()))
        else:
            events.append((m.start(), "open", m.end()))
    pos = 0
    while True:
        i = doc.find(P_CLOSE, pos)
        if i < 0:
            break
        events.append((i, "close", i + len(P_CLOSE)))
        pos = i + 1
    events.sort()
    stack, out = [], []
    for at, kind, end in events:
        if kind == "empty":
            out.append((at, end, ""))
        elif kind == "open":
            stack.append(at)
        elif stack:
            start = stack.pop()
            body = doc[start:end]
            out.append((start, end, "".join(unescape(t) for t in T_TEXT.findall(body))))
    out.sort()
    return out


SIGN_LABEL = re.compile(r"^\s*(signature|sign\s+here|signed|sign|by)\b", re.I)
OTHER_LABEL = re.compile(r"^\s*(date|dated|name|printed\s+name|print\s+name|title|company|address|email|phone)\b", re.I)
MAX_LINE_CHARS = 80


def cell_of(doc, at):
    """(start, end) of the table cell containing offset `at`, or None."""
    start = max(doc.rfind("<w:tc>", 0, at), doc.rfind("<w:tc ", 0, at))
    if start < 0 or doc.rfind("</w:tc>", 0, at) > start:
        return None
    end = doc.find("</w:tc>", at)
    return (start, end + len("</w:tc>")) if end >= 0 else None


def label_left_of(doc, cell):
    """Text of the previous cell in the same table row, or None."""
    prev_end = doc.rfind("</w:tc>", 0, cell[0])
    row = max(doc.rfind("<w:tr>", 0, cell[0]), doc.rfind("<w:tr ", 0, cell[0]))
    if prev_end < 0 or prev_end < row:
        return None
    prev_start = max(doc.rfind("<w:tc>", 0, prev_end), doc.rfind("<w:tc ", 0, prev_end))
    return "".join(unescape(t) for t in T_TEXT.findall(doc[prev_start:prev_end])).strip()


# A signing field's label on its own: "Signature", "Sign here:", "By:".
LABEL_ONLY = re.compile(r"^\s*(signature|sign\s+here|signed|by)\s*[:\-\u2013]?\s*$", re.I)
# Content that is not plain text: an existing signature image, an object, or
# a Word field (its result is regenerated when the field updates, which would
# drop a signature placed inside it).
NON_TEXT = re.compile(r"<w:(drawing|pict|object|fldChar|fldSimple|instrText)\b")
CT_NS = "{http://schemas.openxmlformats.org/package/2006/content-types}"


def default_extensions(types_xml):
    """Extensions with a <Default> content-type mapping, lowercased, read
    with an XML parser so quoting and attribute order do not matter."""
    root = ET.fromstring(types_xml.encode("utf-8") if isinstance(types_xml, str) else types_xml)
    return [d.get("Extension", "").lower() for d in root.iter(CT_NS + "Default")]


FIELD_CHAR = re.compile(r'<w:fldChar\b[^>]*\bw:fldCharType="(begin|end)"')


def field_ranges(doc):
    """Document offsets covered by complex fields (begin .. end), however
    many paragraphs a field spans. Fields nest; the outermost range counts."""
    ranges, depth, opened = [], 0, 0
    for m in FIELD_CHAR.finditer(doc):
        if m.group(1) == "begin":
            if depth == 0:
                opened = m.start()
            depth += 1
        elif depth:
            depth -= 1
            if depth == 0:
                ranges.append((opened, m.end()))
    if depth:
        ranges.append((opened, len(doc)))
    return ranges


def candidates(doc):
    """Paragraphs a signature can go into, top to bottom, numbered from 1:
    a blank drawn with underscores that is not labelled as a date, name or
    title ("Signature: ____", "By: ____"); a signing label on its own line
    ("Signature:"); or the empty cell right of such a label in a table (the
    two-column signature block). Prose that begins like a label ("By signing
    this agreement, ...") is not a line, and a paragraph that already holds a
    picture (a signature already there) is never offered again."""
    out = []
    offered_cells = set()
    fields = field_ranges(doc)
    for index, (start, end, text) in enumerate(paragraphs(doc)):
        stripped = text.strip()
        cell = cell_of(doc, start)
        if NON_TEXT.search(doc[start:end]) or any(a < end and start < b for a, b in fields):
            continue
        if stripped:
            if len(stripped) > MAX_LINE_CHARS or BLOCKED.search(stripped):
                continue
            signing = LABEL_ONLY.search(stripped)
            blank = UNDERSCORES.search(stripped) and not OTHER_LABEL.search(stripped)
            if cell is not None and not UNDERSCORES.search(stripped):
                continue  # a table label: its signature goes in the cell beside it
            if not (signing or blank):
                continue
            out.append(dict(line=len(out) + 1, paragraph=index, text=stripped, kind="line"))
        elif cell is not None:
            # A cell is offered once (its first empty paragraph), and never
            # when any paragraph in it already holds a signature.
            cell_xml = doc[cell[0]:cell[1]]
            if cell in offered_cells or NON_TEXT.search(cell_xml) or readable_text(cell_xml).strip():
                continue
            label = label_left_of(doc, cell)
            if label and LABEL_ONLY.search(label):
                offered_cells.add(cell)
                out.append(dict(line=len(out) + 1, paragraph=index, text=label, kind="cell"))
    return out


def norm(text):
    return " ".join(text.casefold().split())


# Parts a reader sees besides the body, checked for manual-only documents.
VISIBLE_PARTS = re.compile(r"^word/(header\d*|footer\d*|footnotes|endnotes)\.xml$")


# Text as a reader sees it: line breaks, tabs and paragraph ends separate words.
READABLE = re.compile(r"<w:t(?:\s[^>]*)?>([^<]*)</w:t>|<w:(?:br|tab|cr)\b[^>]*>|</w:p>")


def readable_text(xml):
    return "".join(unescape(m.group(1)) if m.group(1) is not None else " " for m in READABLE.finditer(xml))


def refuse_manual_documents(z, doc):
    """Affidavits, sworn statements, perjury declarations and government
    filings are signed by hand, wherever the words appear: the body, a header,
    a footer, or a note, and however the words are broken across lines."""
    texts = [readable_text(doc)]
    for name in z.namelist():
        if VISIBLE_PARTS.match(name):
            texts.append(readable_text(z.read(name).decode("utf-8", "replace")))
    if BLOCKED.search(" ".join(texts)):
        raise ValueError("this document requires manual signing outside Jarvis")
    return texts[0]


def resolve(data):
    _, z, doc = load(data)
    text = refuse_manual_documents(z, doc)
    if not text.strip():
        raise ValueError("document must have readable text for signing review")
    found = candidates(doc)
    target = data["target"]
    if "line" in target:
        match = [c for c in found if c["line"] == target["line"]]
        if not match:
            raise ValueError(f"there is no line {target['line']} in this document ({len(found)} detected)")
        choice = match[0]
    elif "anchor_text" in target:
        phrase = norm(target["anchor_text"])
        if not phrase:
            raise ValueError("anchor text is empty")
        match = [c for c in found if phrase in norm(c["text"])]
        if not match:
            raise ValueError(f'no signature line contains "{target["anchor_text"]}"; run doc sign-lines and choose --line')
        if len(match) > 1:
            where = ", ".join(f'line {c["line"]} ("{c["text"][:40]}")' for c in match)
            raise ValueError(f'"{target["anchor_text"]}" matches {len(match)} lines ({where}); use --line')
        choice = match[0]
    else:
        raise ValueError("Word documents are placed by --anchor-text or --line, not coordinates")
    width = float(data.get("width") or DEFAULT_WIDTH_PT)
    if not 24 <= width <= 432:
        raise ValueError("signature width must be between 24 and 432 points")
    return dict(paragraph=choice["paragraph"], line=choice["line"], text=choice["text"], width=width)


def ink_png(png_bytes):
    """The signature cropped to its visible ink, as PNG bytes and (w, h)."""
    im = Image.open(io.BytesIO(png_bytes))
    if im.format != "PNG" or not 1 <= im.width <= 4096 or not 1 <= im.height <= 4096:
        raise ValueError("signature must be a PNG up to 4096 x 4096")
    im = im.convert("RGBA")
    alpha = im.getchannel("A").point(lambda a: 255 if a > 8 else 0)
    dark = im.convert("L").point(lambda v: 255 if v < 245 else 0)
    from PIL import ImageChops
    box = ImageChops.multiply(alpha, dark).getbbox()
    if box is None:
        raise ValueError("signature image has no visible ink")
    im = im.crop(box)
    out = io.BytesIO()
    im.save(out, "PNG", optimize=False)
    return out.getvalue(), im.size


EXACT_LINE = re.compile(r'<w:spacing\b[^>]*>')
EXACT_ROW = re.compile(r'<w:trHeight\b[^>]*>')


def twips_attr(tag, name):
    m = re.search(rf'\bw:{name}="(-?\d+)"', tag)
    return int(m.group(1)) if m else None


def refuse_clipping(doc, start, end, height_pt):
    """An inline picture taller than a fixed line height (exact paragraph
    spacing) or a fixed table row height is clipped in Word, while the
    preview shows it whole. Refuse such a placement, naming the limit."""
    ppr = re.search(r"<w:pPr>.*?</w:pPr>", doc[start:end], re.S)
    for tag in EXACT_LINE.findall(ppr.group(0) if ppr else ""):
        line = twips_attr(tag, "line")
        if 'w:lineRule="exact"' in tag and line is not None and line / 20 < height_pt:
            raise ValueError(f"the signature line has a fixed height of {line / 20:g} pt, shorter than the "
                             f"{height_pt:.0f} pt signature; use a smaller --width or sign by hand")
    cell = cell_of(doc, start)
    if cell is not None:
        row = max(doc.rfind("<w:tr>", 0, cell[0]), doc.rfind("<w:tr ", 0, cell[0]))
        trpr = re.search(r"<w:trPr>.*?</w:trPr>", doc[row:cell[0]], re.S) if row >= 0 else None
        for tag in EXACT_ROW.findall(trpr.group(0) if trpr else ""):
            val = twips_attr(tag, "val")
            if 'w:hRule="exact"' in tag and val is not None and val / 20 < height_pt:
                raise ValueError(f"the signature cell's row has a fixed height of {val / 20:g} pt, shorter than the "
                                 f"{height_pt:.0f} pt signature; use a smaller --width or sign by hand")


def unique(existing, make):
    n = 1
    while make(n) in existing:
        n += 1
    return make(n)


def drawing_run(rid, docpr_id, cx, cy):
    return (
        '<w:r><w:drawing><wp:inline distT="0" distB="0" distL="0" distR="0" '
        'xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing">'
        f'<wp:extent cx="{cx}" cy="{cy}"/>'
        f'<wp:docPr id="{docpr_id}" name="Jarvis signature"/>'
        '<a:graphic xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">'
        '<a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture">'
        '<pic:pic xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture">'
        '<pic:nvPicPr><pic:cNvPr id="0" name="signature.png"/><pic:cNvPicPr/></pic:nvPicPr>'
        '<pic:blipFill><a:blip xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" '
        f'r:embed="{rid}"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>'
        f'<pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm>'
        '<a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr></pic:pic>'
        '</a:graphicData></a:graphic></wp:inline></w:drawing></w:r>'
    )


T_ELEM = re.compile(r"(<w:t(?:\s[^>]*)?>)([^<]*)(</w:t>)")
R_OPEN = re.compile(r"<w:r(?:\s[^>]*)?>")


def place_in_paragraph(body, drawing, date):
    """Insert the signature into one paragraph's XML. The paragraph's first
    underscore blank (3+ underscores of its text, across however many text
    elements it spans) is removed and the signature put where it began;
    without a blank the signature follows the paragraph's text. The date, if
    any, follows the signature. Only text characters of the blank are
    removed: every other element of every run is kept, in order."""
    date_run = f'<w:r><w:t xml:space="preserve">   {escape(date)}</w:t></w:r>' if date else ""
    insert = drawing + date_run
    if body.endswith("/>"):  # an empty <w:p/>: open it up
        return body[:-2] + ">" + insert + P_CLOSE
    # The paragraph's text with breaks and tabs kept as separators, so a
    # blank never runs across them (the preview reads them the same way).
    elems, texts, starts = [], [], []
    full = ""
    for tok in LAYOUT.finditer(body):
        if tok.group(1) is None:
            full += "\n"
            continue
        m = T_ELEM.match(body, tok.start())
        elems.append(m)
        texts.append(unescape(m.group(2)))
        starts.append(len(full))
        full += texts[-1]
    blank = UNDERSCORES.search(full)
    if not blank:
        close = body.rfind(P_CLOSE)
        return body[:close] + insert + body[close:]
    # Edits run back to front so earlier offsets stay valid.
    edits = []
    first = None
    for m, text, lo in zip(elems, texts, starts):
        hi = lo + len(text)
        a, b = max(lo, blank.start()), min(hi, blank.end())
        if a >= b:
            continue
        head, tail = text[: a - lo], text[b - lo:]
        if first is None:
            first = (m, head, tail)
        else:
            edits.append((m.start(2), m.end(2), escape(tail) if tail is not None else ""))
            if head:
                raise ValueError("signature blank is interleaved with text; sign it by hand")
    m, head, tail = first
    run_start = max((r.start() for r in R_OPEN.finditer(body, 0, m.start()) if body.rfind("</w:r>", r.start(), m.start()) < 0), default=-1)
    run_end = body.find("</w:r>", m.end())
    if run_start < 0 or run_end < 0:
        raise ValueError("signature blank is not inside a text run; sign it by hand")
    run_open = R_OPEN.match(body, run_start).group(0)
    rpr = RPR.search(body, run_start, m.start())
    rpr = rpr.group(0) if rpr else ""
    t_open = '<w:t xml:space="preserve">'
    before = body[run_start:m.start()] + (t_open + escape(head) + "</w:t>" if head else "") + "</w:r>"
    after_children = body[m.end():run_end]
    for s_, e_, text in sorted((x for x in edits if m.end() <= x[0] and x[1] <= run_end), reverse=True):
        after_children = after_children[:s_ - m.end()] + text + after_children[e_ - m.end():]
    after = ""
    if tail or after_children.strip():
        after = run_open + rpr + (t_open + escape(tail) + "</w:t>" if tail else "") + after_children + "</w:r>"
    rebuilt = before + insert + after
    replacements = [x for x in edits if not (m.end() <= x[0] and x[1] <= run_end)]
    replacements.append((run_start, run_end + len("</w:r>"), rebuilt))
    out = body
    for s, e, text in sorted(replacements, key=lambda x: x[0], reverse=True):
        out = out[:s] + text + out[e:]
    return out


def sign(data):
    raw, z, doc = load(data)
    refuse_manual_documents(z, doc)
    placement = data["placement"]
    date = data.get("date")
    if date and not re.fullmatch(r"\d{4}-\d{2}-\d{2}", date):
        raise ValueError("date must be YYYY-MM-DD")
    found = {c["paragraph"]: c for c in candidates(doc)}
    index = placement["paragraph"]
    if index not in found:
        raise ValueError("the chosen paragraph is no longer a signature line; propose again")
    paras = paragraphs(doc)
    start, end, _ = paras[index]
    png, (w, h) = ink_png(base64.b64decode(data["png"], validate=True))
    width = float(placement["width"])
    cx = int(round(width * EMU_PER_PT))
    cy = int(round(width * h / w * EMU_PER_PT))
    refuse_clipping(doc, start, end, width * h / w)
    names = set(z.namelist())
    media = unique(names, lambda n: f"word/media/jarvis-signature-{n}.png")
    rels = z.read(RELS).decode("utf-8") if RELS in names else (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"></Relationships>')
    rels = re.sub(r"<Relationships(\s[^>]*)?/>", lambda m: f"<Relationships{m.group(1) or ''}></Relationships>", rels)
    rid = unique(set(re.findall(r'Id="([^"]+)"', rels)), lambda n: f"rIdJarvisSignature{n}")
    docpr = max([int(x) for x in re.findall(r'<wp:docPr[^>]*\bid="(\d+)"', doc)] + [0]) + 1
    drawing = drawing_run(rid, docpr, cx, cy)
    signed_doc = doc[:start] + place_in_paragraph(doc[start:end], drawing, date) + doc[end:]
    rel = (f'<Relationship Id="{rid}" '
           'Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" '
           f'Target="media/{media.rsplit("/", 1)[1]}"/>')
    close = rels.rfind("</Relationships>")
    if close < 0:
        raise ValueError("document relationships are unreadable")
    signed_rels = rels[:close] + rel + rels[close:]
    types = z.read(TYPES).decode("utf-8")
    if "png" not in default_extensions(types):
        close = types.rfind("</Types>")
        types = types[:close] + '<Default Extension="png" ContentType="image/png"/>' + types[close:]
    changed = {DOC: signed_doc.encode("utf-8"), RELS: signed_rels.encode("utf-8"), TYPES: types.encode("utf-8")}
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w") as dst:
        for info in z.infolist():
            data_bytes = changed.pop(info.filename, None)
            if data_bytes is None:
                data_bytes = z.read(info.filename)
            dst.writestr(info, data_bytes, compress_type=info.compress_type)
        for name, data_bytes in [(RELS, changed.pop(RELS, None)), (media, png)]:
            if data_bytes is None:
                continue
            info = zipfile.ZipInfo(name, date_time=FIXED_TIME)
            info.compress_type = zipfile.ZIP_DEFLATED if name != media else zipfile.ZIP_STORED
            dst.writestr(info, data_bytes)
    signed = out.getvalue()
    verify(raw, signed, start, end, media)
    return signed


def verify(original, signed, start, end, media):
    """Refuse output that changed anything but the chosen paragraph, the image
    relationship, the png content type and the new image."""
    a = zipfile.ZipFile(io.BytesIO(original))
    b = zipfile.ZipFile(io.BytesIO(signed))
    if b.testzip() is not None:
        raise ValueError("signed package failed its integrity check; nothing was signed")
    before = set(a.namelist())
    after = set(b.namelist())
    if not before <= after or after - before - {media, RELS}:
        raise ValueError("signing would add or remove document parts; nothing was signed")
    for name in before - {DOC, RELS, TYPES}:
        if a.read(name) != b.read(name):
            raise ValueError(f"signing would change {name}; nothing was signed")
    old, new = a.read(DOC).decode("utf-8"), b.read(DOC).decode("utf-8")
    tail = len(old) - end
    if new[:start] != old[:start] or new[len(new) - tail:] != old[end:]:
        raise ValueError("signing would change text outside the signature line; nothing was signed")
    for name in (DOC, RELS, TYPES):
        ET.fromstring(b.read(name))
    exts = default_extensions(b.read(TYPES))
    if len(exts) != len(set(exts)):
        raise ValueError("signing would duplicate a content-type mapping; nothing was signed")


def lines(data):
    _, _, doc = load(data)
    return dict(lines=candidates(doc))


# The paragraph as laid out: breaks start a row, tabs are wide spaces.
LAYOUT = re.compile(r"<w:t(?:\s[^>]*)?>([^<]*)</w:t>|<w:(br|cr|tab)\b[^>]*>")


def preview_layout(doc, para_span):
    """(rows before the signature row, text before the signature on its row,
    text after it on its row, rows after) for the signed paragraph, so the
    preview places the signature where the published document does, across
    the paragraph's own line breaks and tabs."""
    start, end, _ = para_span
    text = "".join(
        unescape(m.group(1)) if m.group(1) is not None else ("\n" if m.group(2) in ("br", "cr") else "    ")
        for m in LAYOUT.finditer(doc[start:end]))
    blank = UNDERSCORES.search(text)
    head, tail = (text[:blank.start()], text[blank.end():]) if blank else (text, "")
    head_rows, tail_rows = head.split("\n"), tail.split("\n")
    return head_rows[:-1], head_rows[-1], tail_rows[0], tail_rows[1:]


def preview(data):
    """A picture of the signature line as it will read once signed: the
    paragraphs before and after for context, and the line itself with the
    signature drawn where it is inserted. Word documents have no fixed page
    layout without a renderer, so this shows placement, not pagination."""
    _, _, doc = load(data)
    paras = paragraphs(doc)
    index = data["placement"]["paragraph"]
    before = paras[index - 1][2].strip() if index > 0 else ""
    after = paras[index + 1][2].strip() if index + 1 < len(paras) else ""
    rows_before, head, tail, rows_after = preview_layout(doc, paras[index])
    png, (w, h) = ink_png(base64.b64decode(data["png"], validate=True))
    sig = Image.open(io.BytesIO(png)).convert("RGBA")
    sig_w = int(data["placement"]["width"] * 2)
    sig = sig.resize((sig_w, max(1, int(sig_w * h / w))))
    font = ImageFont.load_default(size=28)
    small = ImageFont.load_default(size=22)
    date = data.get("date") or ""
    # Size the canvas to the whole signed line, so the signature and date are
    # never cropped out of the preview the owner approves.
    measure = ImageDraw.Draw(Image.new("RGB", (1, 1)))
    line_width = 40 + int(measure.textlength(head.strip(), font=font)) + 16 + sig.width + 16
    if date:
        line_width += int(measure.textlength(date, font=font)) + 16
    if tail.strip():
        line_width += int(measure.textlength(tail.strip(), font=font))
    for row in rows_before + rows_after:
        line_width = max(line_width, 40 + int(measure.textlength(row, font=font)))
    width = max(1400, line_width + 40)
    row_h = 40
    height = 120 + row_h * len(rows_before) + sig.height + row_h * len(rows_after) + 120
    img = Image.new("RGB", (width, height), "white")
    d = ImageDraw.Draw(img)
    d.text((40, 20), before[:110], fill=(130, 130, 130), font=small)
    for n, row in enumerate(rows_before):
        d.text((40, 70 + n * row_h), row, fill="black", font=font)
    y = 70 + row_h * len(rows_before) + sig.height
    x = 40
    d.text((x, y - 30), head.strip(), fill="black", font=font)
    x += int(d.textlength(head.strip(), font=font)) + 16
    img.paste(sig, (x, y - sig.height), sig)
    x += sig.width + 16
    if date:
        d.text((x, y - 30), date, fill="black", font=font)
        x += int(d.textlength(date, font=font)) + 16
    if tail.strip():
        d.text((x, y - 30), tail.strip(), fill="black", font=font)
    for n, row in enumerate(rows_after):
        d.text((40, y + 10 + n * row_h), row, fill="black", font=font)
    d.text((40, y + 30 + row_h * len(rows_after)), after[:110], fill=(130, 130, 130), font=small)
    out = io.BytesIO()
    img.save(out, "PNG")
    return out.getvalue()


if __name__ == "__main__":
    try:
        data = json.loads(sys.stdin.buffer.read(40 * 1024 * 1024))
        op = data.pop("op")
        if op == "lines":
            result = json.dumps(lines(data)).encode()
        elif op == "resolve":
            result = json.dumps(resolve(data)).encode()
        elif op == "sign":
            result = sign(data)
        elif op == "preview":
            result = preview(data)
        else:
            raise ValueError("unknown signing operation")
        sys.stdout.buffer.write(result)
    except Exception as error:
        # Do not print input, signature data, or parser dumps to logs/model context.
        print("Word signing validation failed: " + (str(error) if isinstance(error, ValueError) else type(error).__name__), file=sys.stderr)
        sys.exit(1)
