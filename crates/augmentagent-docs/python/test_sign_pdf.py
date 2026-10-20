import base64
import io
import os
import re
import subprocess
import tempfile
import time
import unittest
from PIL import Image
from reportlab.pdfgen import canvas
from pypdf import PdfReader, PdfWriter
from pypdf.generic import ArrayObject, DecodedStreamObject, DictionaryObject, FloatObject, NameObject
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


def rewrite(data, writer):
    """Replace the PDF in a request with the writer's current document."""
    out = io.BytesIO()
    writer.write(out)
    data['pdf'] = base64.b64encode(out.getvalue()).decode()
    return out.getvalue()


def shared_xobjects_request(pages=100, shared=2000):
    """Every page references the same `shared` Form XObject objects, the shape
    that made the un-memoised verifier do O(pages * shared) stream hashes."""
    data = request(pages)
    writer = PdfWriter(clone_from=io.BytesIO(base64.b64decode(data['pdf'])))
    font = writer.pages[0]['/Resources']['/Font'].raw_get('/F1')
    xobjects = DictionaryObject()
    for i in range(shared):
        form = DecodedStreamObject()
        form.set_data(b'')
        form.update({
            NameObject('/Type'): NameObject('/XObject'),
            NameObject('/Subtype'): NameObject('/Form'),
            NameObject('/BBox'): ArrayObject([FloatObject(0), FloatObject(0), FloatObject(1), FloatObject(1)]),
            NameObject('/Matrix'): ArrayObject([FloatObject(v) for v in (1, 0, 0, 1, 0, 0)]),
            NameObject('/Resources'): DictionaryObject({
                NameObject('/ProcSet'): ArrayObject([NameObject('/PDF'), NameObject('/Text')]),
                NameObject('/Font'): DictionaryObject({NameObject('/F1'): font}),
            }),
        })
        xobjects[NameObject(f'/Sh{i}')] = writer._add_object(form)
    shared_ref = writer._add_object(xobjects)
    for page in writer.pages:
        resources = DictionaryObject(dict(page['/Resources'].get_object().items()))
        resources[NameObject('/XObject')] = shared_ref
        page[NameObject('/Resources')] = resources
    data['page'] = 1
    rewrite(data, writer)
    return data


def inherited_resources_request(pages=3):
    """No page carries its own /Resources; they all inherit one indirect
    dictionary from the /Pages node. Returns the request, its PDF bytes and
    the object number of that shared dictionary."""
    data = request(pages)
    writer = PdfWriter(clone_from=io.BytesIO(base64.b64decode(data['pdf'])))
    shared = DictionaryObject(dict(writer.pages[0]['/Resources'].get_object().items()))
    ref = writer._add_object(shared)
    writer._root_object['/Pages'][NameObject('/Resources')] = ref
    for page in writer.pages:
        del page[NameObject('/Resources')]
    data['page'] = 1
    return data, rewrite(data, writer), ref.idnum


def render(pdf, page):
    """Rasterise one page with poppler's pdftoppm. The binary is a hard
    requirement of this suite: without it the rendering check must fail, not
    quietly pass."""
    with tempfile.TemporaryDirectory() as directory:
        path = os.path.join(directory, 'in.pdf')
        with open(path, 'wb') as handle:
            handle.write(pdf)
        return subprocess.run(
            ['pdftoppm', '-r', '72', '-f', str(page), '-l', str(page), path],
            check=True, capture_output=True).stdout


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
            # The signature is a Form XObject, so the alpha-masked image sits in
            # that form's own resources.
            form = result.pages[-1]['/Resources']['/XObject']['/JarvisSignature'].get_object()
            images = form['/Resources']['/XObject'].get_object()
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

    def test_shared_xobjects_sign_quickly(self):
        data = shared_xobjects_request()
        start = time.monotonic()
        signed = sign_pdf.process(data, True)
        elapsed = time.monotonic() - start
        self.assertIn('2026-10-12', PdfReader(io.BytesIO(signed)).pages[0].extract_text())
        self.assertLess(elapsed, 5)

    def test_pages_inheriting_resources_are_untouched(self):
        data, pdf, shared = inherited_resources_request()
        self.assertIn(b'/Resources %d 0 R' % shared, pdf)
        # Read the page objects straight off the page tree: pypdf's `pages`
        # property copies inherited attributes onto each page, which would
        # mask a fixture that stopped exercising inheritance.
        structure = PdfReader(io.BytesIO(pdf), strict=True)
        kids = structure.trailer['/Root']['/Pages']['/Kids']
        for kid in kids:
            self.assertNotIn('/Resources', kid.get_object())

        signed = sign_pdf.process(data, True)
        self.assertEqual(signed[:len(pdf)], pdf)
        appended = signed[len(pdf):]
        self.assertIsNone(re.search(rb'(?m)^%d 0 obj' % shared, appended))
        for kid in kids[1:]:
            self.assertIsNone(re.search(rb'(?m)^%d 0 obj' % kid.idnum, appended))

        result = PdfReader(io.BytesIO(signed), strict=True)
        signed_kids = result.trailer['/Root']['/Pages']['/Kids']
        for kid in signed_kids[1:]:
            self.assertNotIn('/Resources', kid.get_object())
        self.assertIn('2026-10-12', result.pages[0].extract_text())
        for i in range(1, len(result.pages)):
            self.assertNotIn('2026-10-12', result.pages[i].extract_text())
            self.assertEqual(render(signed, i + 1), render(pdf, i + 1))


if __name__ == '__main__':
    unittest.main()
