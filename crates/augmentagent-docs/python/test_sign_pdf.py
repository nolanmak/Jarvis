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
            result = PdfReader(io.BytesIO(sign_pdf.process(data, 'sign')))
            self.assertEqual(len(result.pages), n)
            self.assertIn('Agreement for services', result.pages[-1].extract_text())
            self.assertIn('2026-10-12', result.pages[-1].extract_text())
            # The signature is a Form XObject (#1507) wrapping the image.
            forms = result.pages[-1]['/Resources']['/XObject'].get_object()
            form = forms['/JarvisSignature'].get_object()
            images = form['/Resources']['/XObject'].get_object()
            self.assertTrue(any('/SMask' in obj.get_object() for obj in images.values()))
            if n > 1:
                self.assertNotIn('2026-10-12', result.pages[0].extract_text())

    def test_invalid_placement_and_documents(self):
        for change in ({'page':0}, {'page':2}, {'x':-1}, {'y':790}, {'width':0}, {'x':float('nan')}, {'pdf':'broken'}, {'png':'broken'}):
            with self.subTest(change=change), self.assertRaises(Exception):
                sign_pdf.process(dict(request(), **change), 'sign')
        for text in ('Affidavit', 'Declaration under penalty of perjury', 'Government filing'):
            with self.assertRaisesRegex(ValueError, 'manual signing'):
                sign_pdf.process(request(text=text), 'sign')

    def test_rules_are_measured_from_the_rendered_page(self):
        # 100 dpi raster of a 612x792 page: one 1 px dark rule, one thick bar
        # (a box, not a line), one short dash (not a line), light-gray rule.
        im = Image.new('L', (850, 1100), 255)
        for x in range(100, 600):
            im.putpixel((x, 300), 0)
        for y in range(500, 520):
            for x in range(100, 600):
                im.putpixel((x, y), 0)
        for x in range(100, 150):
            im.putpixel((x, 700), 0)
        for x in range(100, 600):
            im.putpixel((x, 900), 211)
        raster = io.BytesIO()
        im.save(raster, 'PNG')
        rules = sign_pdf.find_rules(raster.getvalue(), 100, 612, 792)
        self.assertEqual([r['line'] for r in rules], [1, 2])
        self.assertAlmostEqual(rules[0]['y'], 792 - 300.5 * 0.72, places=1)
        self.assertAlmostEqual(rules[0]['x0'], 72, places=1)
        self.assertAlmostEqual(rules[1]['y'], 792 - 900.5 * 0.72, places=1)
        with self.assertRaisesRegex(ValueError, 'does not match'):
            sign_pdf.find_rules(raster.getvalue(), 300, 612, 792)

    def test_ink_bounds_ignore_transparent_and_white_padding(self):
        im = Image.new('RGBA', (300, 100), (255, 255, 255, 255))
        for x in range(40, 260):
            im.putpixel((x, 70), (0, 0, 0, 255))
        self.assertEqual(sign_pdf.ink_bounds(im), (40, 70, 260, 71))
        im = Image.new('RGBA', (300, 100), (0, 0, 0, 0))
        with self.assertRaisesRegex(ValueError, 'no visible ink'):
            sign_pdf.ink_bounds(im)


if __name__ == '__main__':
    unittest.main()
