"""Private stdin/stdout worker; never accepts filesystem paths or network URLs."""
import base64
import datetime
import hashlib
import io
import json
import math
import re
import sys
from PIL import Image, ImageChops
from pypdf import PdfReader, PdfWriter
from pypdf.generic import ArrayObject, DecodedStreamObject, DictionaryObject, FloatObject, NameObject
from reportlab.lib.utils import ImageReader
from reportlab.pdfbase.pdfmetrics import stringWidth
from reportlab.pdfgen import canvas


ACTIVE_ACTIONS = {'/JavaScript', '/Launch'}


def resolve(value):
    return value.get_object() if hasattr(value, 'get_object') else value


def is_active_action(action, depth=0):
    action = resolve(action)
    if depth > 32:
        return True
    if isinstance(action, list):
        return any(is_active_action(a, depth + 1) for a in action)
    if not isinstance(action, dict):
        return False
    if action.get('/S') in ACTIVE_ACTIONS or '/JS' in action:
        return True
    return '/Next' in action and is_active_action(action['/Next'], depth + 1)


def triggers_active(holder):
    holder = resolve(holder)
    if not isinstance(holder, dict):
        return False
    if is_active_action(holder.get('/OpenAction')) or is_active_action(holder.get('/A')):
        return True
    aa = resolve(holder.get('/AA'))
    return isinstance(aa, dict) and any(is_active_action(v) for v in aa.values())


def refuse_active_content(reader):
    root = reader.trailer['/Root']
    names = resolve(root.get('/Names'))
    form = resolve(root.get('/AcroForm'))
    if (triggers_active(root) or (isinstance(names, dict) and '/JavaScript' in names)
            or (isinstance(form, dict) and '/XFA' in form)):
        raise ValueError('PDFs with scripts, launch actions or XFA forms require manual signing')
    holders = list(reader.pages)
    for page in reader.pages:
        holders.extend(resolve(page.get('/Annots')) or [])
    # Walk form fields and bookmarks (both can carry actions). Each indirect
    # object is visited once, so cycles terminate; an absurd size fails closed.
    pending = list(resolve(form.get('/Fields')) or []) if isinstance(form, dict) else []
    outlines = resolve(root.get('/Outlines'))
    if isinstance(outlines, dict) and '/First' in outlines:
        pending.append(outlines.raw_get('/First'))
    seen = set()
    while pending:
        item = pending.pop()
        key = getattr(item, 'idnum', None)
        if key is not None:
            if key in seen:
                continue
            seen.add(key)
        if len(holders) > 100_000:
            raise ValueError('PDF structure is too large to check; sign it manually')
        node = resolve(item)
        if not isinstance(node, dict):
            continue
        holders.append(node)
        pending.extend(resolve(node.get('/Kids')) or [])
        for link in ('/First', '/Next'):
            if link in node:
                pending.append(node.raw_get(link))
    if any(triggers_active(h) for h in holders):
        raise ValueError('PDFs with scripts, launch actions or XFA forms require manual signing')


def inherited_resources(page):
    node, depth = page, 0
    while isinstance(node, dict) and depth < 64:
        if '/Resources' in node:
            return resolve(node['/Resources'])
        node, depth = resolve(node.get('/Parent')), depth + 1
    return DictionaryObject()


def stamp_page(writer, page, overlay):
    """Draw `overlay` on `page` without touching any object another page could
    share. pypdf's merge_page rewrites the page's existing content streams in
    place (and nulls array elements), which corrupts every other page that
    references the same stream. Here the signature is a new Form XObject, the
    page gets a new /Contents array that only *references* its original
    streams, and a private copy of its resource dictionary."""
    width, height = float(page.mediabox.width), float(page.mediabox.height)
    form = DecodedStreamObject()
    form.set_data(overlay.get_contents().get_data())
    form.update({
        NameObject('/Type'): NameObject('/XObject'),
        NameObject('/Subtype'): NameObject('/Form'),
        NameObject('/BBox'): ArrayObject([FloatObject(0), FloatObject(0), FloatObject(width), FloatObject(height)]),
        NameObject('/Resources'): overlay['/Resources'].clone(writer),
    })
    form_ref = writer._add_object(form)

    resources = inherited_resources(page)
    xobjects = resolve(resources.get('/XObject')) or DictionaryObject()
    name, n = '/JarvisSignature', 0
    while NameObject(name) in xobjects:
        n += 1
        name = f'/JarvisSignature{n}'
    private_x = DictionaryObject(dict(xobjects.items()))
    private_x[NameObject(name)] = form_ref
    private = DictionaryObject(dict(resources.items()))
    private[NameObject('/XObject')] = private_x

    raw = page.get('/Contents')
    contents = resolve(raw)
    if contents is None:
        parts = []
    elif isinstance(contents, list):
        parts = list(contents)  # the elements are references; copy the array, never edit it
    elif hasattr(raw, 'idnum'):
        parts = [raw]
    else:
        parts = [writer._add_object(contents)]

    def stream(data):
        s = DecodedStreamObject()
        s.set_data(data)
        return writer._add_object(s)

    # Balance any unclosed graphics state the original content leaves behind.
    page[NameObject('/Contents')] = ArrayObject(
        [stream(b'q\n'), *parts, stream(f'Q\nq {name} Do Q\n'.encode())])
    page[NameObject('/Resources')] = private


def canonical(value, depth=0):
    """Reader-independent structure of a PDF value: references by object
    number plus their resolved content (a few levels deep), streams by their
    raw bytes. Two readers of the same bytes produce equal results."""
    if hasattr(value, 'idnum'):
        if depth >= 6:
            return ('ref', value.idnum, value.generation)
        return ('ref', value.idnum, value.generation, canonical(value.get_object(), depth + 1))
    if isinstance(value, dict):
        items = tuple(sorted((str(k), canonical(v, depth + 1)) for k, v in value.items()))
        raw = getattr(value, '_data', None)
        return ('stream', items, hashlib.sha256(raw).hexdigest()) if raw is not None else ('dict', items)
    if isinstance(value, list):
        return ('array', tuple(canonical(v, depth + 1) for v in value))
    return ('value', str(value))


def page_fingerprint(page):
    contents = page.get_contents()
    return (contents.get_data() if contents is not None else b'',
            canonical(inherited_resources(page)),
            [float(v) for v in page.mediabox], page.rotation)


def verify_other_pages(original, signed, signed_index):
    if len(original.pages) != len(signed.pages):
        raise ValueError('signing changed the page count')
    for i, (a, b) in enumerate(zip(original.pages, signed.pages)):
        if i != signed_index and page_fingerprint(a) != page_fingerprint(b):
            raise ValueError(f'signing would change page {i + 1}; nothing was signed')


RULE_MIN_PT = 72      # a signature line is at least an inch long
RULE_MAX_THICK_PT = 3  # thicker dark bands are boxes or images, not lines
BELOW_PT = 40          # text this far under a line is its label
GAP_PT = 1.5           # anchored ink sits this far above the line's top edge
INSET_PT = 12
DEFAULT_INK_PT = 180
DARK = 235             # gray levels below this count as ink (light-gray rules too)


def find_rules(raster, dpi, page_w, page_h):
    """Horizontal rules on a rendered page, in PDF points from the bottom-left.
    Measuring the rendered page treats stroked paths, thin filled rectangles,
    underscore runs, form XObjects and any CTM alike: it is what a reader sees."""
    im = Image.open(io.BytesIO(raster))
    im.load()
    im = im.convert('L')
    w, h = im.size
    scale = 72 / dpi
    if abs(w * scale - page_w) > 2 or abs(h * scale - page_h) > 2:
        raise ValueError('page raster does not match the page size')
    table = bytes(35 if v < DARK else 46 for v in range(256))  # '#' dark, '.' light
    data = im.tobytes().translate(table)
    run = re.compile(rb'#(?:#|\.{1,2}#)*')  # bridge anti-aliasing seams between glyphs
    min_px = RULE_MIN_PT / scale
    bands = []  # [top_row, bottom_row, x0_px, x1_px]
    open_bands = []
    for y in range(h):
        row = data[y * w:(y + 1) * w]
        current = []
        for m in run.finditer(row):
            if m.end() - m.start() < min_px:
                continue
            x0, x1 = m.start(), m.end()
            band = next((b for b in open_bands if min(b[3], x1) - max(b[2], x0) > 0.5 * (x1 - x0)), None)
            if band:
                band[1], band[2], band[3] = y, min(band[2], x0), max(band[3], x1)
            else:
                band = [y, y, x0, x1]
                bands.append(band)
            current.append(band)
        open_bands = current
    rules = []
    for top, bottom, x0, x1 in bands:
        if (bottom - top + 1) * scale > RULE_MAX_THICK_PT:
            continue
        t, b = page_h - top * scale, page_h - (bottom + 1) * scale
        rules.append(dict(x0=round(x0 * scale, 2), x1=round(x1 * scale, 2), top=round(t, 2),
                          bottom=round(b, 2), y=round((t + b) / 2, 2)))
    rules.sort(key=lambda r: (-r['y'], r['x0']))
    for i, r in enumerate(rules, 1):
        r['line'] = i
    return rules


def text_rows(words, page_h):
    """pdftotext -bbox words (top-left origin) grouped into baseline rows."""
    rows = []
    for x0, top, x1, bottom, text in sorted(words, key=lambda w: (w[3], w[0])):
        y0, y1 = page_h - bottom, page_h - top
        row = next((r for r in rows if abs(r['y0'] - y0) < 1.5), None)
        if row is None:
            row = dict(y0=y0, words=[])
            rows.append(row)
        row['words'].append(dict(x0=x0, x1=x1, y0=y0, y1=y1, text=text))
    for r in rows:
        r['words'].sort(key=lambda w: w['x0'])
    return rows


def norm(text):
    return ' '.join(text.casefold().split())


def label_below(rule, rows):
    near = [r for r in rows
            if rule['bottom'] + 1 >= max(w['y1'] for w in r['words']) >= rule['bottom'] - BELOW_PT]
    for r in sorted(near, key=lambda r: -max(w['y1'] for w in r['words'])):
        words = [w for w in r['words'] if w['x1'] > rule['x0'] and w['x0'] < rule['x1']]
        if words:
            return ' '.join(w['text'] for w in words)
    return None


def page_layout(data, page):
    layout = data.get('layout')
    if not isinstance(layout, dict):
        raise ValueError('page layout is required')
    page_w, page_h = float(page.mediabox.width), float(page.mediabox.height)
    raster = base64.b64decode(layout['raster'], validate=True)
    rules = find_rules(raster, int(layout['dpi']), page_w, page_h)
    rows = text_rows([tuple(w) for w in layout['words']], page_h)
    for r in rules:
        r['below'] = label_below(r, rows)
    return rules, rows, raster


RULE_TEXT = re.compile(r'^[_\-\u2010-\u2015.\u00b7\u2026]+:?$')


def marks_in(raster, dpi, page_h, box, rules):
    """Dark pixels a reader would see inside `box` that are not a detected
    rule: images, scans and outlined text that pdftotext cannot report."""
    im = Image.open(io.BytesIO(raster))
    im.load()
    im = im.convert('L')
    k = dpi / 72
    x0, x1 = max(0, int(box[0] * k)), min(im.width, int(math.ceil(box[2] * k)))
    y0, y1 = max(0, int((page_h - box[3]) * k)), min(im.height, int(math.ceil((page_h - box[1]) * k)))
    if x1 <= x0 or y1 <= y0:
        return None
    region = im.crop((x0, y0, x1, y1)).load()
    for py in range(y1 - y0):
        y_pt = page_h - (y0 + py + 0.5) / k
        on_rule = [r for r in rules if r['bottom'] - 1.5 / k <= y_pt <= r['top'] + 1.5 / k]
        for px in range(x1 - x0):
            if region[px, py] < DARK:
                x_pt = (x0 + px + 0.5) / k
                if not any(r['x0'] - 1 <= x_pt <= r['x1'] + 1 for r in on_rule):
                    return round(y_pt, 1)
    return None


def anchored_rule(target, rules, rows):
    if 'line' in target:
        index = target['line']
        match = [r for r in rules if r['line'] == index]
        if not match:
            raise ValueError(f'there is no line {index} on this page ({len(rules)} detected)')
        return match[0]
    phrase = norm(target['anchor_text'])
    if not phrase:
        raise ValueError('anchor text is empty')
    hits = []
    for row in rows:
        tokens = [norm(w['text']) for w in row['words']]
        want = phrase.split()
        for i in range(len(tokens) - len(want) + 1):
            if tokens[i:i + len(want)] == want:
                span = row['words'][i:i + len(want)]
                hits.append(dict(x0=span[0]['x0'], x1=span[-1]['x1'], top=max(w['y1'] for w in span)))
    if not hits:
        raise ValueError(f'anchor text "{target["anchor_text"]}" is not on this page')
    found = []
    for hit in hits:
        above = [r for r in rules if r['x1'] > hit['x0'] and r['x0'] < hit['x1']
                 and hit['top'] - 0.5 <= r['bottom'] <= hit['top'] + BELOW_PT]
        if above:
            nearest = min(above, key=lambda r: r['bottom'])
            if nearest not in found:
                found.append(nearest)
    if not found:
        raise ValueError(f'no signature line directly above "{target["anchor_text"]}"; '
                         'run doc sign-lines and choose --line, or give explicit --x/--y')
    if len(found) > 1:
        where = ', '.join(f'line {r["line"]} at y={r["y"]}' for r in found)
        raise ValueError(f'anchor text "{target["anchor_text"]}" is under {len(found)} lines ({where}); use --line')
    return found[0]


def ink_bounds(im):
    """Visible-ink bounding box of the signature image, in pixels (top-left)."""
    alpha = im.getchannel('A')
    luma = im.convert('L')
    visible = Image.eval(alpha, lambda a: 255 if a > 8 else 0)
    dark = Image.eval(luma, lambda v: 255 if v < 245 else 0)
    bbox = ImageChops.multiply(visible, dark).getbbox()
    if bbox is None:
        raise ValueError('signature image has no visible ink')
    return bbox


def overlaps(a, b, tol=0.25):
    return (min(a[2], b[2]) - max(a[0], b[0]) > tol) and (min(a[3], b[3]) - max(a[1], b[1]) > tol)


def resolve_placement(data, page, im):
    """Turn a placement request into the exact image box to draw, aligning the
    signature's visible ink (not its bitmap, which may carry padding) and
    refusing anything that would cover text or cross a line."""
    page_w, page_h = float(page.mediabox.width), float(page.mediabox.height)
    rules, rows, raster = page_layout(data, page)
    target = data['target']
    if not isinstance(target, dict) or len(target) != 1:
        raise ValueError('placement needs exactly one of anchor text, line or coordinates')
    ix0, iy0, ix1, iy1 = ink_bounds(im)
    aspect = (iy1 - iy0) / (ix1 - ix0)
    anchor = None
    if 'ink' in target:
        ink = target['ink']
        x, y, width = (float(ink[k]) for k in ('x', 'y', 'width'))
    else:
        rule = anchored_rule(target, rules, rows)
        x = rule['x0'] + INSET_PT
        y = rule['top'] + GAP_PT
        width = data.get('width')
        width = float(min(DEFAULT_INK_PT, rule['x1'] - x) if width is None else width)
        anchor = dict(line=rule['line'], x0=rule['x0'], x1=rule['x1'], top=rule['top'],
                      text=target.get('anchor_text'))
    height = width * aspect
    if not all(math.isfinite(v) for v in (x, y, width)) or width <= 0:
        raise ValueError('signature placement must be finite with a positive width')
    s = width / (ix1 - ix0)
    box = dict(x=x - ix0 * s, y=y - (im.height - iy1) * s, width=im.width * s)
    date = data.get('date')
    date_box = None
    if date:
        # Beside the ink, on its baseline: digits and hyphens have no descenders.
        date_x, date_y = x + width + 12, y
        box.update(date_x=round(date_x, 3), date_y=round(date_y, 3))
        date_box = (date_x, date_y, date_x + stringWidth(date, 'Helvetica', 10), date_y + 7.5)
    ink_box = (x, y, x + width, y + height)
    # A bitmap without transparency paints its whole box (white padding included).
    opaque = ink_box if im.getextrema()[3][0] < 255 else (box['x'], box['y'], box['x'] + box['width'], box['y'] + im.height * s)
    for b in [opaque] + ([date_box] if date_box else []):
        if b[0] < 0 or b[1] < 0 or b[2] > page_w or b[3] > page_h:
            raise ValueError('signature or date falls outside the page')
        for row in rows:
            for w in row['words']:
                # An underscore run is the signature line itself; the rule
                # check below covers it, its glyph box would always "overlap".
                if RULE_TEXT.match(w['text']):
                    continue
                wx1 = w['x1']
                if '__' in w['text']:  # "Signature:____" is one word: keep only its label
                    starts = [r['x0'] for r in rules if r['x0'] > w['x0'] and r['x0'] < w['x1']
                              and r['bottom'] - 3 <= w['y1'] and r['top'] + 3 >= w['y0']]
                    wx1 = min(starts, default=wx1)
                if overlaps(b, (w['x0'], w['y0'], wx1, w['y1'])):
                    raise ValueError(f'signature would overlap the text "{w["text"]}" at y={round(w["y0"], 1)}; '
                                     'choose a line with doc sign-lines')
        for r in rules:
            if overlaps(b, (r['x0'], r['bottom'], r['x1'], r['top']), 0.5):
                raise ValueError(f'signature would cross line {r["line"]} at y={r["y"]}')
        mark = marks_in(raster, int(data['layout']['dpi']), page_h, b, rules)
        if mark is not None:
            raise ValueError(f'signature would cover existing marks (an image or graphics) at y={mark}; '
                             'choose another line or a smaller --width')
    box = {k: round(v, 3) for k, v in box.items()}
    box['ink'] = dict(x=round(x, 3), y=round(y, 3), width=round(width, 3), height=round(height, 3))
    if anchor:
        box['anchor'] = anchor
    return box


def load(data):
    pdf = base64.b64decode(data['pdf'], validate=True)
    if len(pdf) > 8 * 1024 * 1024:
        raise ValueError('document or signature exceeds size limit')
    reader = PdfReader(io.BytesIO(pdf), strict=True)
    if reader.is_encrypted or not 1 <= len(reader.pages) <= 100:
        raise ValueError('requires an unencrypted PDF with 1–100 pages')
    return pdf, reader


def target_page(data, reader, measured):
    page_number = data['page']
    if not isinstance(page_number, int) or not 1 <= page_number <= len(reader.pages):
        raise ValueError('page number is outside the document')
    page = reader.pages[page_number - 1]
    if page.rotation or float(page.mediabox.left) != 0 or float(page.mediabox.bottom) != 0:
        raise ValueError('rotated/offset pages are not supported')
    # Poppler renders the CropBox; measuring needs it to be the page.
    if measured and [float(v) for v in page.cropbox] != [float(v) for v in page.mediabox]:
        raise ValueError('cropped pages cannot be measured; give explicit coordinates or sign manually')
    return page_number, page


MAX_MEASURED_PT = 1440  # 20 in; a 300 dpi render stays well under PIL's bomb limit


def info(data):
    _, reader = load(data)
    sizes = [[float(p.mediabox.width), float(p.mediabox.height)] for p in reader.pages]
    return dict(pages=len(reader.pages), sizes=sizes, max_pt=MAX_MEASURED_PT)


def lines(data):
    _, reader = load(data)
    _, page = target_page(data, reader, True)
    rules, _, _ = page_layout(data, page)
    return dict(pages=len(reader.pages), lines=rules)


def process(data, op):
    pdf, reader = load(data)
    png = base64.b64decode(data['png'], validate=True)
    if len(png) > 1024 * 1024:
        raise ValueError('document or signature exceeds size limit')
    text = '\n'.join(p.extract_text() for p in reader.pages)
    if not text.strip():
        raise ValueError('PDF must have readable text for signing review')
    if re.search(r'\b(affidavit|sworn\s+statement|penalty\s+of\s+perjury|government\s+filing)\b', text, re.I):
        raise ValueError('this document requires manual signing outside Jarvis')
    # Owner uploads routinely carry a view-setting /OpenAction or a plain form;
    # those are inert. Fail closed only for content that executes or reflows.
    refuse_active_content(reader)
    page_number, page = target_page(data, reader, op == 'resolve')
    im = Image.open(io.BytesIO(png))
    if im.format != 'PNG' or not 1 <= im.width <= 4096 or not 1 <= im.height <= 4096:
        raise ValueError('signature must be a PNG up to 4096 x 4096')
    im.load()
    im = im.convert('RGBA')
    date = data.get('date')
    if date:
        if not re.fullmatch(r'\d{4}-\d{2}-\d{2}', date):
            raise ValueError('date must be YYYY-MM-DD')
        datetime.date.fromisoformat(date)
    if op == 'resolve':
        placement = resolve_placement(data, page, im)
        placement['pages'] = len(reader.pages)
        return placement
    x, y, width = (float(data[k]) for k in ('x', 'y', 'width'))
    height = width * im.height / im.width
    if 'ink' in data:
        # Resolved (ink-aligned) placement: bounds and overlap were checked at
        # resolve time; transparent padding may legitimately hang off the page.
        if not all(math.isfinite(v) for v in (x, y, width)) or width <= 0:
            raise ValueError('signature placement must be finite with a positive width')
        date_at = (float(data['date_x']), float(data['date_y'])) if date else None
    else:
        # Proposals made before #1510 address the image box; keep their meaning.
        if (not all(math.isfinite(v) for v in (x,y,width,height)) or width <= 0 or x < 0 or y < (14 if date else 0)
                or x + max(width, 60 if date else 0) > float(page.mediabox.width) or y + height > float(page.mediabox.height)):
            raise ValueError('signature or date falls outside the page')
        date_at = (x, y - 12)
    overlay = io.BytesIO()
    c = canvas.Canvas(overlay, pagesize=(float(page.mediabox.width), float(page.mediabox.height)), invariant=1)
    c.drawImage(ImageReader(im), x, y, width=width, height=height, mask='auto')
    if date:
        c.setFont('Helvetica', 10)
        c.drawString(*date_at, date)
    c.save()
    # ISO 32000-1 §7.5.6 incremental update: the original bytes are kept
    # verbatim and only the changed page plus its new content are appended,
    # so the signed file is the input file, edited, never a re-encoding.
    writer = PdfWriter(io.BytesIO(pdf), incremental=True)
    stamp_page(writer, writer.pages[page_number - 1], PdfReader(overlay).pages[0])
    output = io.BytesIO()
    writer.write(output)
    signed = output.getvalue()
    if len(signed) <= len(pdf) or signed[:len(pdf)] != pdf:
        raise ValueError('signing would alter the original document bytes')
    verify_other_pages(reader, PdfReader(io.BytesIO(signed), strict=True), page_number - 1)
    return signed


if __name__ == '__main__':
    try:
        data = json.loads(sys.stdin.buffer.read(40 * 1024 * 1024))
        op = data.pop('op')
        if op not in ('info', 'lines', 'resolve', 'sign'):
            raise ValueError('unknown signing operation')
        result = info(data) if op == 'info' else lines(data) if op == 'lines' else process(data, op)
        sys.stdout.buffer.write(result if op == 'sign' else json.dumps(result).encode())
    except Exception as error:
        # Do not print input, signature data, or parser dumps to logs/model context.
        print('PDF signing validation failed: ' + (str(error) if isinstance(error, ValueError) else type(error).__name__), file=sys.stderr)
        sys.exit(1)
