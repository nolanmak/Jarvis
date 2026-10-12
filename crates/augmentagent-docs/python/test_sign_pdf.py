import base64
import io
import unittest
from PIL import Image
from reportlab.pdfgen import canvas
from pypdf import PdfReader
import sign_pdf


def request(pages=1, text='Agreement for services'):
    source = io.BytesIO()
    c = canvas.Canvas(source, pagesize=(612, 792))
    for _ in range(pages):
        c.drawString(72, 720, text)
        c.showPage()
    c.save()
    image = io.BytesIO()
    im = Image.new('RGBA', (200, 50), (0, 0, 0, 0))
    for x in range(10, 190):
        im.putpixel((x, 25), (0, 0, 0, 255))
    im.save(image, 'PNG')
    return dict(pdf=base64.b64encode(source.getvalue()).decode(), png=base64.b64encode(image.getvalue()).decode(),
                page=pages, x=72, y=80, width=144, date='2026-10-12')


class SigningTests(unittest.TestCase):
    def test_one_and_multiple_pages_preserve_content_and_alpha(self):
        for n in (1, 3):
            data = request(n)
            info = sign_pdf.process(data, False)
            self.assertEqual(info['pages'], n)
            result = PdfReader(io.BytesIO(sign_pdf.process(data, True)))
            self.assertEqual(len(result.pages), n)
            self.assertIn('Agreement for services', result.pages[-1].extract_text())
            self.assertIn('2026-10-12', result.pages[-1].extract_text())
            images = result.pages[-1]['/Resources']['/XObject'].get_object()
            self.assertTrue(any('/SMask' in obj.get_object() for obj in images.values()))
            if n > 1:
                self.assertNotIn('2026-10-12', result.pages[0].extract_text())

    def test_invalid_placement_and_documents(self):
        for change in ({'page':0}, {'page':2}, {'x':-1}, {'y':790}, {'width':0}, {'x':float('nan')}, {'pdf':'broken'}, {'png':'broken'}):
            with self.subTest(change=change), self.assertRaises(Exception):
                sign_pdf.process(dict(request(), **change), True)
        for text in ('Affidavit', 'Declaration under penalty of perjury', 'Government filing'):
            with self.assertRaisesRegex(ValueError, 'manual signing'):
                sign_pdf.process(request(text=text), False)

if __name__ == '__main__':
    unittest.main()
