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
    # Generated PDFs have ordinary page boxes; fail closed for imported active content.
    if '/AcroForm' in reader.trailer['/Root'] or '/OpenAction' in reader.trailer['/Root']:
        raise ValueError('interactive PDFs require manual signing')
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
    writer = PdfWriter()
    writer.append(reader)
    writer.pages[page_number - 1].merge_page(PdfReader(overlay).pages[0])
    output = io.BytesIO()
    writer.write(output)
    return output.getvalue()


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
