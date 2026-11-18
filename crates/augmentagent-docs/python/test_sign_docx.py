import base64
import io
import re
import unittest
import zipfile

from PIL import Image

import sign_docx

NS = ('xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" '
      'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"')
TYPES = ('<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
         '<Default Extension="xml" ContentType="application/xml"/>{extra}</Types>')
RELS = '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"></Relationships>'


def docx(body, rels=RELS, types_extra=""):
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("[Content_Types].xml", TYPES.format(extra=types_extra))
        z.writestr("word/document.xml",
                   f'<?xml version="1.0" encoding="UTF-8"?><w:document {NS}><w:body>{body}</w:body></w:document>')
        if rels is not None:
            z.writestr("word/_rels/document.xml.rels", rels)
    return out.getvalue()


def signature_png():
    im = Image.new("RGBA", (200, 50), (0, 0, 0, 0))
    for x in range(10, 190):
        im.putpixel((x, 25), (0, 0, 0, 255))
    out = io.BytesIO()
    im.save(out, "PNG")
    return out.getvalue()


def b64(raw):
    return base64.b64encode(raw).decode()


def sign_line(raw, line=1, date=None):
    data = dict(docx=b64(raw), png=b64(signature_png()))
    placement = sign_docx.resolve(dict(data, target=dict(line=line), width=None))
    return sign_docx.sign(dict(data, placement=placement, date=date))


def part(raw, name):
    return zipfile.ZipFile(io.BytesIO(raw)).read(name).decode()


def para(*runs):
    return "<w:p>" + "".join(runs) + "</w:p>"


def run(text, extra_before="", extra_after=""):
    return f'<w:r><w:rPr><w:b/></w:rPr>{extra_before}<w:t xml:space="preserve">{text}</w:t>{extra_after}</w:r>'


def table_row(label, empty_cell):
    return (f'<w:tbl><w:tr><w:tc><w:p><w:r><w:t>{label}</w:t></w:r></w:p></w:tc>'
            f'<w:tc>{empty_cell}</w:tc></w:tr></w:tbl>')


class WordSigningTests(unittest.TestCase):
    def test_signature_replaces_the_blank_and_keeps_the_date(self):
        raw = docx(para(run("Agreement text.")) + para(run("Signature: ________")))
        doc = part(sign_line(raw, date="2026-11-18"), "word/document.xml")
        self.assertEqual(doc.count("<w:drawing>"), 1)
        self.assertNotIn("___", doc)
        self.assertIn("2026-11-18", doc)

    def test_an_empty_self_closing_cell_paragraph_is_a_signing_cell(self):
        # Review of #2241: Word writes an empty cell's paragraph as <w:p/>.
        raw = docx(para(run("Terms.")) + table_row("Signature:", '<w:p w:rsidR="00A1"/>'))
        lines = sign_docx.lines(dict(docx=b64(raw)))["lines"]
        self.assertEqual([(l["kind"], l["text"]) for l in lines], [("cell", "Signature:")])
        doc = part(sign_line(raw), "word/document.xml")
        self.assertIn('<w:p w:rsidR="00A1"><w:r><w:drawing>', doc)
        self.assertNotIn('<w:p w:rsidR="00A1"/>', doc)

    def test_a_blank_split_across_runs_is_removed_entirely(self):
        # Review of #2241: the preview showed the whole blank gone.
        raw = docx(para(run("Terms.")) + para(run("Signature: ___"), run("____"), run(" (Client)")))
        doc = part(sign_line(raw), "word/document.xml")
        self.assertNotIn("_", doc)
        text = "".join(re.findall(r"<w:t[^>]*>([^<]*)</w:t>", doc))
        self.assertIn("Signature: ", text)
        self.assertIn(" (Client)", text)
        self.assertLess(doc.index("Signature: "), doc.index("<w:drawing>"))
        self.assertLess(doc.index("<w:drawing>"), doc.index(" (Client)"))

    def test_other_run_children_are_kept_in_order(self):
        # Review of #2241: a line break or tab in the blank's run survives.
        raw = docx(para(run("Terms.")) + para(run("Signature: _____", extra_before="<w:tab/>", extra_after="<w:br/>")))
        doc = part(sign_line(raw), "word/document.xml")
        self.assertEqual(doc.count("<w:tab/>"), 1)
        self.assertEqual(doc.count("<w:br/>"), 1)
        self.assertLess(doc.index("<w:tab/>"), doc.index("<w:drawing>"))
        self.assertLess(doc.index("<w:drawing>"), doc.index("<w:br/>"))
        self.assertIn("<w:b/>", doc[doc.index("<w:drawing>"):], "the run's formatting carries on after the signature")

    def test_a_self_closing_relationships_part_is_signed(self):
        # Review of #2241.
        rels = '<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"/>'
        raw = docx(para(run("Terms.")) + para(run("Signature: _____")), rels=rels)
        signed_rels = part(sign_line(raw), "word/_rels/document.xml.rels")
        self.assertEqual(signed_rels.count("<Relationship "), 1)
        self.assertIn("</Relationships>", signed_rels)

    def test_an_existing_png_mapping_is_not_duplicated_whatever_its_attribute_order(self):
        # Review of #2241.
        raw = docx(para(run("Terms.")) + para(run("Signature: _____")),
                   types_extra='<Default ContentType="image/png" Extension="png"/>')
        types = part(sign_line(raw), "[Content_Types].xml")
        self.assertEqual(len(re.findall(r'Extension="png"', types)), 1)

    def test_everything_but_the_signed_line_is_unchanged(self):
        raw = docx(para(run("Terms.")) + para(run("Signature: _____")) + para(run("After.")))
        signed = sign_line(raw)
        a, b = zipfile.ZipFile(io.BytesIO(raw)), zipfile.ZipFile(io.BytesIO(signed))
        before, after = a.read("word/document.xml").decode(), b.read("word/document.xml").decode()
        cut = before.index("<w:p><w:r><w:rPr><w:b/></w:rPr><w:t xml:space=\"preserve\">Signature")
        self.assertEqual(after[:cut], before[:cut])
        self.assertTrue(after.endswith(before[before.index("After."):]))
        self.assertEqual(signed, sign_line(raw), "deterministic")

    def test_macro_documents_and_prose_mentions_are_refused(self):
        out = io.BytesIO()
        with zipfile.ZipFile(io.BytesIO(docx(para(run("Signature: ___")))), "r") as src, \
                zipfile.ZipFile(out, "w") as dst:
            for info in src.infolist():
                dst.writestr(info, src.read(info))
            dst.writestr("word/vbaProject.bin", b"macro")
        with self.assertRaisesRegex(ValueError, "macro-enabled"):
            sign_docx.lines(dict(docx=b64(out.getvalue())))
        prose = docx(para(run("This agreement starts on signature by both parties.")))
        self.assertEqual(sign_docx.lines(dict(docx=b64(prose)))["lines"], [])


if __name__ == "__main__":
    unittest.main()
