"""Private stdin/stdout worker; never accepts filesystem paths or network URLs."""
import base64
import datetime
import io
import json
import math
import re
import sys
from PIL import Image
from pypdf import PdfReader, PdfWriter
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
    fields = list(resolve(form.get('/Fields')) or []) if isinstance(form, dict) else []
    while fields and len(holders) < 100_000:
        field = resolve(fields.pop())
        holders.append(field)
        if isinstance(field, dict):
            fields.extend(resolve(field.get('/Kids')) or [])
    if any(triggers_active(h) for h in holders):
        raise ValueError('PDFs with scripts, launch actions or XFA forms require manual signing')


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
    writer.pages[page_number - 1].merge_page(PdfReader(overlay).pages[0])
    output = io.BytesIO()
    writer.write(output)
    signed = output.getvalue()
    if len(signed) <= len(pdf) or signed[:len(pdf)] != pdf:
        raise ValueError('signing would alter the original document bytes')
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
