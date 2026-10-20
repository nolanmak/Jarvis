"""Private stdin/stdout worker; never accepts filesystem paths or network URLs."""
import base64
import datetime
import hashlib
import io
import json
import math
import re
import sys
from PIL import Image
from pypdf import PdfReader, PdfWriter
from pypdf.generic import ArrayObject, DecodedStreamObject, DictionaryObject, FloatObject, NameObject
from reportlab.lib.utils import ImageReader
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


def process(data, sign):
    pdf = base64.b64decode(data['pdf'], validate=True)
    png = base64.b64decode(data['png'], validate=True)
    if len(pdf) > 8 * 1024 * 1024 or len(png) > 1024 * 1024:
        raise ValueError('document or signature exceeds size limit')
    reader = PdfReader(io.BytesIO(pdf), strict=True)
    if reader.is_encrypted or not 1 <= len(reader.pages) <= 100:
        raise ValueError('requires an unencrypted PDF with 1–100 pages')
    text = '\n'.join(p.extract_text() for p in reader.pages)
    if not text.strip():
        raise ValueError('PDF must have readable text for signing review')
    if re.search(r'\b(affidavit|sworn\s+statement|penalty\s+of\s+perjury|government\s+filing)\b', text, re.I):
        raise ValueError('this document requires manual signing outside Jarvis')
    # Owner uploads routinely carry a view-setting /OpenAction or a plain form;
    # those are inert. Fail closed only for content that executes or reflows.
    refuse_active_content(reader)
    page_number = data['page']
    if not isinstance(page_number, int) or not 1 <= page_number <= len(reader.pages):
        raise ValueError('page number is outside the document')
    page = reader.pages[page_number - 1]
    if page.rotation or float(page.mediabox.left) != 0 or float(page.mediabox.bottom) != 0:
        raise ValueError('rotated/offset pages are not supported')
    im = Image.open(io.BytesIO(png))
    if im.format != 'PNG' or not 1 <= im.width <= 4096 or not 1 <= im.height <= 4096:
        raise ValueError('signature must be a PNG up to 4096 x 4096')
    im.load()
    im = im.convert('RGBA')
    x, y, width = (float(data[k]) for k in ('x', 'y', 'width'))
    height = width * im.height / im.width
    date = data.get('date')
    if date:
        if not re.fullmatch(r'\d{4}-\d{2}-\d{2}', date):
            raise ValueError('date must be YYYY-MM-DD')
        datetime.date.fromisoformat(date)
    if (not all(math.isfinite(v) for v in (x,y,width,height)) or width <= 0 or x < 0 or y < (14 if date else 0)
            or x + max(width, 60 if date else 0) > float(page.mediabox.width) or y + height > float(page.mediabox.height)):
        raise ValueError('signature or date falls outside the page')
    info = dict(pages=len(reader.pages), height=height)
    if not sign:
        return info
    overlay = io.BytesIO()
    c = canvas.Canvas(overlay, pagesize=(float(page.mediabox.width), float(page.mediabox.height)))
    c.drawImage(ImageReader(im), x, y, width=width, height=height, mask='auto')
    if date:
        c.setFont('Helvetica', 10)
        c.drawString(x, y - 12, date)
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
        data = json.loads(sys.stdin.buffer.read(14 * 1024 * 1024))
        signed = data.pop('sign', False)
        result = process(data, signed)
        sys.stdout.buffer.write(result if signed else json.dumps(result).encode())
    except Exception as error:
        # Do not print input, signature data, or parser dumps to logs/model context.
        print('PDF signing validation failed: ' + (str(error) if isinstance(error, ValueError) else type(error).__name__), file=sys.stderr)
        sys.exit(1)
