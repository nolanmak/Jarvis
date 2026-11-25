import base64
import io
import subprocess
import tempfile
import time
from pathlib import Path
from unittest.mock import patch
import unittest
from PIL import Image
from reportlab.pdfgen import canvas
from pypdf import PdfReader, PdfWriter
from pypdf.generic import (ArrayObject, DecodedStreamObject, DictionaryObject,
                           IndirectObject, NameObject)
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


def shared_resources_request(pages=3, xobjects=2, inherited=False):
    data = request(pages)
    writer = PdfWriter(clone_from=PdfReader(io.BytesIO(base64.b64decode(data['pdf']))))
    forms = DictionaryObject()
    ids = set()
    for index in range(xobjects):
        form = DecodedStreamObject()
        form.set_data(b'0 0 10 10 re f\n')
        form.update({NameObject('/Type'): NameObject('/XObject'),
                     NameObject('/Subtype'): NameObject('/Form'),
                     NameObject('/BBox'): ArrayObject(writer.pages[0].mediabox),
                     NameObject('/Resources'): DictionaryObject()})
        ref = writer._add_object(form)
        forms[NameObject(f'/Shared{index}')] = ref
        ids.add(ref.idnum)
    resources = DictionaryObject(dict(writer.pages[0]['/Resources'].items()))
    resources[NameObject('/XObject')] = writer._add_object(forms)
    resources_ref = writer._add_object(resources)
    parent = writer.pages[0]['/Parent']
    parent_ref = parent.indirect_reference
    if inherited:
        parent[NameObject('/Resources')] = resources_ref
    for page in writer.pages:
        if inherited:
            del page['/Resources']
        else:
            page[NameObject('/Resources')] = resources_ref
        drawing = DecodedStreamObject()
        drawing.set_data(b'q 1 0 0 1 100 100 cm /Shared0 Do Q\n')
        page[NameObject('/Contents')] = ArrayObject([page.raw_get('/Contents'), writer._add_object(drawing)])
    output = io.BytesIO()
    writer.write(output)
    data.update(pdf=base64.b64encode(output.getvalue()).decode(), page=1)
    return data, ids, (parent_ref.idnum, resources_ref.idnum)


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

    def test_later_page_signs_when_pages_share_a_type0_font(self):
        # #2081: Chrome/Skia and Word exports give pages a shared Type0 font
        # whose /DescendantFonts holds an indirect reference. Text extraction
        # resolved it in place on the original reader, so signing page 3
        # reported that page 1 would change and signed nothing.
        from pypdf import PdfWriter
        from pypdf.generic import (ArrayObject, DecodedStreamObject, DictionaryObject,
                                   NameObject, NumberObject, TextStringObject)
        data = request(3)
        w = PdfWriter(clone_from=PdfReader(io.BytesIO(base64.b64decode(data['pdf']))))
        cid = w._add_object(DictionaryObject({
            NameObject('/Type'): NameObject('/Font'),
            NameObject('/Subtype'): NameObject('/CIDFontType2'),
            NameObject('/BaseFont'): NameObject('/AAAAAA+Test'),
            NameObject('/CIDSystemInfo'): DictionaryObject({
                NameObject('/Registry'): TextStringObject('Adobe'),
                NameObject('/Ordering'): TextStringObject('Identity'),
                NameObject('/Supplement'): NumberObject(0)}),
            NameObject('/DW'): NumberObject(556),
        }))
        font = w._add_object(DictionaryObject({
            NameObject('/Type'): NameObject('/Font'),
            NameObject('/Subtype'): NameObject('/Type0'),
            NameObject('/BaseFont'): NameObject('/AAAAAA+Test'),
            NameObject('/Encoding'): NameObject('/Identity-H'),
            NameObject('/DescendantFonts'): ArrayObject([cid]),
        }))
        for page in w.pages:
            fonts = page['/Resources'].get_object()['/Font'].get_object()
            fonts[NameObject('/T0')] = font
            text = DecodedStreamObject()
            text.set_data(b'BT /T0 10 Tf 72 700 Td <0001> Tj ET')
            page[NameObject('/Contents')] = ArrayObject([page.raw_get('/Contents'), w._add_object(text)])
        out = io.BytesIO()
        w.write(out)
        source = out.getvalue()
        data['pdf'] = base64.b64encode(source).decode()
        for width in (144, 60):
            with self.subTest(width=width):
                signed = sign_pdf.process(dict(data, width=width), 'sign')
                self.assertTrue(signed.startswith(source))
                self.assertIn('2026-10-12', PdfReader(io.BytesIO(signed)).pages[2].extract_text())

    def test_verifier_resolves_shared_objects_once_per_reader(self):
        data, ids, _ = shared_resources_request(pages=4)
        source = base64.b64decode(data['pdf'])
        original = PdfReader(io.BytesIO(source))
        signed = PdfReader(io.BytesIO(source))
        calls = {}
        get_object = IndirectObject.get_object
        def counted(ref):
            if ref.idnum in ids:
                key = (id(ref.pdf), ref.idnum, ref.generation)
                calls[key] = calls.get(key, 0) + 1
            return get_object(ref)
        with patch.object(IndirectObject, 'get_object', counted):
            sign_pdf.verify_other_pages(original, signed, 0)
        self.assertEqual(len(calls), 2 * len(ids))
        self.assertEqual(set(calls.values()), {1}, calls)

    def test_hundred_pages_sharing_two_thousand_xobjects_sign_under_five_seconds(self):
        data, _, _ = shared_resources_request(pages=100, xobjects=2000)
        start = time.monotonic()
        signed = sign_pdf.process(data, 'sign')
        elapsed = time.monotonic() - start
        self.assertLess(elapsed, 5, f'shared-resource signing took {elapsed:.3f}s')
        self.assertTrue(signed.startswith(base64.b64decode(data['pdf'])))

    def test_inherited_resources_preserve_sibling_pixels_and_original_objects(self):
        data, _, shared_ids = shared_resources_request(inherited=True)
        source = base64.b64decode(data['pdf'])
        # Inspect the raw page tree before pypdf flattens inherited attributes.
        root = PdfReader(io.BytesIO(source)).trailer['/Root']['/Pages']
        self.assertIn('/Resources', root)
        self.assertTrue(all('/Resources' not in ref.get_object() for ref in root['/Kids']))
        signed = sign_pdf.process(data, 'sign')
        self.assertTrue(signed.startswith(source))
        result = PdfReader(io.BytesIO(signed))
        for object_id in shared_ids:
            self.assertLess(result.xref[0][object_id], len(source), 'shared object redefined in append')
        self.assertIn('2026-10-12', result.pages[0].extract_text())
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for label, pdf in [('original', source), ('signed', signed)]:
                path = root / (label + '.pdf')
                path.write_bytes(pdf)
                subprocess.run(['pdftoppm', '-r', '72', '-f', '2', '-l', '3', '-png',
                                str(path), str(root / label)], check=True, capture_output=True)
            for page in (2, 3):
                with Image.open(root / f'original-{page}.png') as a, Image.open(root / f'signed-{page}.png') as b:
                    self.assertEqual(a.size, b.size)
                    self.assertEqual(a.tobytes(), b.tobytes())

    def test_verifier_cache_cannot_hide_an_unsigned_resource_change(self):
        data, ids, _ = shared_resources_request()
        source = base64.b64decode(data['pdf'])
        original = PdfReader(io.BytesIO(source))
        signed = PdfReader(io.BytesIO(source))
        shared = signed.get_object(next(iter(ids)))
        shared.set_data(b'0 0 50 50 re f\n')
        with self.assertRaisesRegex(ValueError, 'change page 2'):
            sign_pdf.verify_other_pages(original, signed, 0)

    def test_invalid_placement_and_documents(self):
        for change in ({'page':0}, {'page':2}, {'x':-1}, {'y':790}, {'width':0}, {'x':float('nan')}, {'pdf':'broken'}, {'png':'broken'}):
            with self.subTest(change=change), self.assertRaises(Exception):
                sign_pdf.process(dict(request(), **change), 'sign')
        for text in ('Affidavit', 'Declaration under penalty of perjury', 'Government filing'):
            with self.assertRaisesRegex(ValueError, 'manual signing'):
                sign_pdf.process(request(text=text), 'sign')

    def test_cropped_pages_still_sign_with_image_box_placements(self):
        # Proposals made before #1510 never measured the page; a CropBox must
        # not strand them at approval.
        from pypdf import PdfWriter
        from pypdf.generic import RectangleObject
        data = request()
        w = PdfWriter(clone_from=PdfReader(io.BytesIO(base64.b64decode(data['pdf']))))
        w.pages[0].cropbox = RectangleObject([10, 10, 600, 780])
        out = io.BytesIO()
        w.write(out)
        data['pdf'] = base64.b64encode(out.getvalue()).decode()
        self.assertTrue(sign_pdf.process(data, 'sign').startswith(base64.b64decode(data['pdf'])))

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
