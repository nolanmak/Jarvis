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


def docx(body, rels=RELS, types_extra="", styles=None):
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
        if styles is not None:
            z.writestr("word/styles.xml", f'<?xml version="1.0" encoding="UTF-8"?><w:styles {NS}>{styles}</w:styles>')
        z.writestr("[Content_Types].xml", TYPES.format(extra=types_extra))
        z.writestr("word/document.xml",
                   f'<?xml version="1.0" encoding="UTF-8"?><w:document {NS}><w:body>{body}</w:body></w:document>')
        if rels is not None:
            z.writestr("word/_rels/document.xml.rels", rels)
    return out.getvalue()


def signature_png():
    # Ink 180 x 30 px, about 24 pt tall at the default 144 pt width, like a
    # real handwritten signature.
    im = Image.new("RGBA", (200, 50), (0, 0, 0, 0))
    for x in range(10, 190):
        im.putpixel((x, 10 + (x % 30)), (0, 0, 0, 255))
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

    def test_a_bare_self_closing_cell_paragraph_is_a_signing_cell(self):
        # Review of #2241: <w:p/> with no attributes at all.
        raw = docx(para(run("Terms.")) + table_row("Signature:", "<w:p/>"))
        lines = sign_docx.lines(dict(docx=b64(raw)))["lines"]
        self.assertEqual([(l["kind"], l["text"]) for l in lines], [("cell", "Signature:")])
        doc = part(sign_line(raw), "word/document.xml")
        self.assertIn("<w:p><w:r><w:drawing>", doc)

    def test_character_references_in_the_signed_line_are_preserved(self):
        # Review of #2241: &#65;lice must still read Alice after signing.
        raw = docx(para(run("Terms.")) + para(run("Signature (&#65;lice &amp; B&#x6F;b): _____ end")))
        doc = part(sign_line(raw), "word/document.xml")
        text = "".join(sign_docx.unescape(t) for t in re.findall(r"<w:t[^>]*>([^<]*)</w:t>", doc))
        self.assertIn("Signature (Alice & Bob): ", text)
        self.assertIn(" end", text)
        self.assertNotIn("&amp;#", doc)

    def test_references_are_decoded_once(self):
        # Review of #2241: &#38;lt; is the literal text "&lt;", not "<".
        self.assertEqual(sign_docx.unescape("&#38;lt;Client&#38;gt; &amp;amp; &#x41;"), "&lt;Client&gt; &amp; A")
        raw = docx(para(run("Terms.")) + para(run("Signature &#38;lt;Client&#38;gt;: _____")))
        doc = part(sign_line(raw), "word/document.xml")
        text = "".join(sign_docx.unescape(t) for t in re.findall(r"<w:t[^>]*>([^<]*)</w:t>", doc))
        self.assertIn("Signature &lt;Client&gt;: ", text)

    def test_the_preview_shows_the_whole_signed_line(self):
        # Review of #2241: a long label or wide signature was cropped away.
        label = "Signature of the authorized representative of Example Client LLC: "
        raw = docx(para(run("Terms.")) + para(run(label + "_____ (seal)")))
        data = dict(docx=b64(raw), png=b64(signature_png()))
        placement = sign_docx.resolve(dict(data, target=dict(line=1), width=432))
        png = sign_docx.preview(dict(data, placement=placement, date="2026-11-18"))
        im = Image.open(io.BytesIO(png)).convert("L")
        box = Image.eval(im, lambda v: 255 if v < 200 else 0).getbbox()
        self.assertIsNotNone(box)
        self.assertLess(box[2], im.width - 10, "nothing is drawn up to the right edge")

    def test_a_cell_already_signed_is_not_offered_again(self):
        # Review of #2241: an existing signature image has no text.
        raw = docx(para(run("Terms.")) + table_row("Signature:", "<w:p/>"))
        once = sign_line(raw)
        self.assertEqual(sign_docx.lines(dict(docx=b64(once)))["lines"], [])
        with self.assertRaisesRegex(ValueError, "no line 1"):
            sign_line(once)

    def test_manual_only_words_in_a_header_or_footer_are_refused(self):
        # Review of #2241: the exclusion read only the body.
        out = io.BytesIO()
        with zipfile.ZipFile(io.BytesIO(docx(para(run("Terms.")) + para(run("Signature: _____")))), "r") as src, \
                zipfile.ZipFile(out, "w") as dst:
            for info in src.infolist():
                dst.writestr(info, src.read(info))
            dst.writestr("word/footer1.xml", f'<w:ftr {NS}><w:p><w:r><w:t>Signed under penalty of perjury</w:t></w:r></w:p></w:ftr>')
        with self.assertRaisesRegex(ValueError, "manual signing"):
            sign_line(out.getvalue())

    def test_a_manual_only_phrase_split_by_a_break_or_tab_is_refused(self):
        # Review of #2241: "penalty of<w:br/>perjury" read as "penalty ofperjury".
        for sep in ("<w:br/>", "<w:tab/>"):
            raw = docx(para(f'<w:r><w:t>I declare under penalty of</w:t>{sep}<w:t>perjury that this is true.</w:t></w:r>')
                       + para(run("Signature: _____")))
            with self.subTest(sep=sep), self.assertRaisesRegex(ValueError, "manual signing"):
                sign_line(raw)
        raw = docx(para(run("I declare under penalty of")) + para(run("perjury that this is true."))
                   + para(run("Signature: _____")))
        with self.assertRaisesRegex(ValueError, "manual signing"):
            sign_line(raw)

    def test_a_cell_is_offered_once_and_never_after_it_is_signed(self):
        # Review of #2241: two empty paragraphs in one cell meant two offers.
        raw = docx(para(run("Terms.")) + table_row("Signature:", "<w:p/><w:p/>"))
        self.assertEqual(len(sign_docx.lines(dict(docx=b64(raw)))["lines"]), 1)
        once = sign_line(raw)
        self.assertEqual(sign_docx.lines(dict(docx=b64(once)))["lines"], [])

    def test_the_preview_follows_line_breaks_in_the_signed_line(self):
        # Review of #2241: "Signature:<w:br/>_____" signs below the label.
        raw = docx(para(run("Terms.")) + para('<w:r><w:t>Signature:</w:t><w:br/><w:t>_____</w:t><w:tab/><w:t>(seal)</w:t></w:r>'))
        doc = part(raw, "word/document.xml")
        span = sign_docx.paragraphs(doc)[1]
        self.assertEqual(sign_docx.preview_layout(doc, span), (["Signature:"], "", "    (seal)", []))
        data = dict(docx=b64(raw), png=b64(signature_png()))
        placement = sign_docx.resolve(dict(data, target=dict(line=1), width=None))
        self.assertTrue(sign_docx.preview(dict(data, placement=placement)).startswith(b"\x89PNG"))
        signed = part(sign_line(raw), "word/document.xml")
        self.assertLess(signed.index("<w:br/>"), signed.index("<w:drawing>"), "signed below the label too")

    def test_a_cell_with_text_in_it_is_not_offered(self):
        # Review of #2241: a typed name above an empty paragraph.
        raw = docx(para(run("Terms.")) + table_row("Signature:", "<w:p><w:r><w:t>J. Example</w:t></w:r></w:p><w:p/>"))
        self.assertEqual(sign_docx.lines(dict(docx=b64(raw)))["lines"], [])

    def test_a_blank_stops_at_a_line_break(self):
        # Review of #2241: "Signature: ___<w:br/>____" removed both blanks.
        raw = docx(para(run("Terms.")) + para('<w:r><w:t>Signature: ___</w:t><w:br/><w:t>____</w:t></w:r>'))
        doc = part(raw, "word/document.xml")
        self.assertEqual(sign_docx.preview_layout(doc, sign_docx.paragraphs(doc)[1]), ([], "Signature: ", "", ["____"]))
        signed = part(sign_line(raw), "word/document.xml")
        self.assertIn("<w:br/><w:t>____</w:t>", signed, "the second blank, after the break, is kept")
        self.assertLess(signed.index("<w:drawing>"), signed.index("<w:br/>"))

    def test_a_blank_inside_a_word_field_is_not_offered(self):
        # Review of #2241: a field result is regenerated on update.
        field = ('<w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText> MERGEFIELD Sig </w:instrText></w:r>'
                 '<w:r><w:fldChar w:fldCharType="separate"/></w:r>' + run("Signature: _____")
                 + '<w:r><w:fldChar w:fldCharType="end"/></w:r>')
        simple = '<w:fldSimple w:instr=" MERGEFIELD Sig ">' + run("Signature: _____") + "</w:fldSimple>"
        for body in (field, simple):
            raw = docx(para(run("Terms.")) + para(body))
            self.assertEqual(sign_docx.lines(dict(docx=b64(raw)))["lines"], [])

    def test_a_single_quoted_png_mapping_is_not_duplicated(self):
        # Review of #2241.
        raw = docx(para(run("Terms.")) + para(run("Signature: _____")),
                   types_extra="<Default Extension='png' ContentType='image/png'/>")
        types = part(sign_line(raw), "[Content_Types].xml")
        self.assertEqual(sign_docx.default_extensions(types).count("png"), 1)

    def test_a_field_spanning_paragraphs_excludes_its_result_paragraphs(self):
        # Review of #2241: begin/separate in one paragraph, end in a later one.
        body = (para(run("Terms."))
                + '<w:p><w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText> IF 1 = 1 </w:instrText></w:r>'
                  '<w:r><w:fldChar w:fldCharType="separate"/></w:r></w:p>'
                + para(run("Signature: _____"))
                + '<w:p><w:r><w:fldChar w:fldCharType="end"/></w:r></w:p>'
                + para(run("By: _____")))
        lines = sign_docx.lines(dict(docx=b64(docx(body))))["lines"]
        self.assertEqual([l["text"] for l in lines], ["By: _____"])

    def test_a_fixed_height_line_or_row_too_short_for_the_signature_is_refused(self):
        # Review of #2241: Word clips an inline picture to an exact height.
        exact = ('<w:p><w:pPr><w:spacing w:line="240" w:lineRule="exact"/></w:pPr>'
                 + run("Signature: _____") + "</w:p>")
        with self.assertRaisesRegex(ValueError, "fixed height of 12 pt"):
            sign_line(docx(para(run("Terms.")) + exact))
        row = ('<w:tbl><w:tr><w:trPr><w:trHeight w:val="300" w:hRule="exact"/></w:trPr>'
               '<w:tc><w:p><w:r><w:t>Signature:</w:t></w:r></w:p></w:tc><w:tc><w:p/></w:tc></w:tr></w:tbl>')
        with self.assertRaisesRegex(ValueError, "row has a fixed height of 15 pt"):
            sign_line(docx(para(run("Terms.")) + row))
        # A fixed height tall enough, or an "at least" height, signs.
        roomy = ('<w:p><w:pPr><w:spacing w:line="1440" w:lineRule="exact"/></w:pPr>'
                 + run("Signature: _____") + "</w:p>")
        self.assertIn("<w:drawing>", part(sign_line(docx(para(run("Terms.")) + roomy)), "word/document.xml"))
        at_least = exact.replace('w:lineRule="exact"', 'w:lineRule="atLeast"')
        self.assertIn("<w:drawing>", part(sign_line(docx(para(run("Terms.")) + at_least)), "word/document.xml"))

    def test_exact_spacing_inherited_from_a_style_or_default_is_checked(self):
        # Review of #2241: the clip check read only the paragraph's own pPr.
        tight = '<w:pPr><w:spacing w:line="240" w:lineRule="exact"/></w:pPr>'
        styled = ('<w:p><w:pPr><w:pStyle w:val="SigLine"/></w:pPr>' + run("Signature: _____") + "</w:p>")
        styles = (f'<w:style w:type="paragraph" w:styleId="Base">{tight}</w:style>'
                  '<w:style w:type="paragraph" w:styleId="SigLine"><w:basedOn w:val="Base"/></w:style>')
        with self.assertRaisesRegex(ValueError, "fixed height of 12 pt"):
            sign_line(docx(para(run("Terms.")) + styled, styles=styles))
        defaults = f"<w:docDefaults><w:pPrDefault>{tight}</w:pPrDefault></w:docDefaults>"
        with self.assertRaisesRegex(ValueError, "fixed height of 12 pt"):
            sign_line(docx(para(run("Terms.")) + para(run("Signature: _____")), styles=defaults))
        # The paragraph's own "at least" spacing overrides an exact style.
        own = ('<w:p><w:pPr><w:pStyle w:val="SigLine"/><w:spacing w:line="240" w:lineRule="atLeast"/></w:pPr>'
               + run("Signature: _____") + "</w:p>")
        self.assertIn("<w:drawing>", part(sign_line(docx(para(run("Terms.")) + own, styles=styles)), "word/document.xml"))

    def test_a_line_with_unresolved_tracked_changes_is_not_offered(self):
        # Review of #2241: a signature inside w:ins would belong to that revision.
        tracked = '<w:p><w:ins w:id="1" w:author="Other">' + run("Signature: _____") + "</w:ins></w:p>"
        deleted = '<w:p>' + run("By: _____") + '<w:del w:id="2" w:author="Other"><w:r><w:delText>x</w:delText></w:r></w:del></w:p>'
        raw = docx(para(run("Terms.")) + tracked + deleted + para(run("Signed: _____")))
        lines = sign_docx.lines(dict(docx=b64(raw)))["lines"]
        self.assertEqual([l["text"] for l in lines], ["Signed: _____"])

    def test_a_signature_cell_in_a_tracked_row_is_not_offered(self):
        # Review of #2241: the row's revision marker lives in w:trPr.
        row = ('<w:tbl><w:tr><w:trPr><w:ins w:id="7" w:author="Other"/></w:trPr>'
               '<w:tc><w:p><w:r><w:t>Signature:</w:t></w:r></w:p></w:tc><w:tc><w:p/></w:tc></w:tr></w:tbl>')
        self.assertEqual(sign_docx.lines(dict(docx=b64(docx(para(run("Terms.")) + row))))["lines"], [])
        plain = row.replace('<w:trPr><w:ins w:id="7" w:author="Other"/></w:trPr>', "")
        self.assertEqual(len(sign_docx.lines(dict(docx=b64(docx(para(run("Terms.")) + plain))))["lines"]), 1)

    def test_prose_that_starts_like_a_label_is_not_a_line(self):
        # Review of #2241.
        raw = docx(para(run("By signing this agreement, you accept these terms."))
                   + para(run("Signed copies are kept by both parties."))
                   + para(run("By:"))
                   + para(run("Signature")))
        lines = sign_docx.lines(dict(docx=b64(raw)))["lines"]
        self.assertEqual([l["text"] for l in lines], ["By:", "Signature"])

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
