"""Write a realistic test agreement .docx with only the standard library."""
import sys
import zipfile

W = 'xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" ' \
    'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"'


def p(text, bold=False, size=None):
    rpr = ""
    if bold or size:
        rpr = "<w:rPr>" + ("<w:b/>" if bold else "") + (f'<w:sz w:val="{size}"/>' if size else "") + "</w:rPr>"
    return f'<w:p><w:r>{rpr}<w:t xml:space="preserve">{text}</w:t></w:r></w:p>'


def cell(text, width=4500):
    inner = f'<w:r><w:t xml:space="preserve">{text}</w:t></w:r>' if text else ""
    return f'<w:tc><w:tcPr><w:tcW w:w="{width}" w:type="dxa"/></w:tcPr><w:p>{inner}</w:p></w:tc>'


def row(*cells):
    return "<w:tr>" + "".join(cell(c) for c in cells) + "</w:tr>"


body = "".join([
    p("Consulting Services Agreement", bold=True, size=32),
    p("This agreement is made between Example Client LLC (“Client”) and Ship Systems, Inc. (“Consultant”)."),
    p("1. Scope. Consultant will deliver the migration plan described in Exhibit A."),
    p("2. Fees. Client will pay $12,000 in two milestones of $6,000 each."),
    p("3. Term. This agreement starts on signature and ends when both milestones are paid."),
    p("Accepted and agreed:", bold=True),
    p("Ship Systems, Inc."),
    p("Signature: ______________________________"),
    p("Name: Nolan Makatche"),
    p("Date: ______________"),
    p(""),
    p("Example Client LLC", bold=True),
    '<w:tbl><w:tblPr><w:tblW w:w="9000" w:type="dxa"/></w:tblPr>'
    + row("Name:", "Jordan Example") + row("Signature:", "") + row("Date:", "") + "</w:tbl>",
    '<w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>',
])

document = ('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
            f'<w:document {W}><w:body>{body}</w:body></w:document>')
types = ('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
         '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
         '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
         '<Default Extension="xml" ContentType="application/xml"/>'
         '<Override PartName="/word/document.xml" '
         'ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>'
         '</Types>')
root_rels = ('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
             '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
             '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>'
             '</Relationships>')
doc_rels = ('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
            '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"></Relationships>')

with zipfile.ZipFile(sys.argv[1], "w", zipfile.ZIP_DEFLATED) as z:
    for name, data in [("[Content_Types].xml", types), ("_rels/.rels", root_rels),
                       ("word/document.xml", document), ("word/_rels/document.xml.rels", doc_rels)]:
        info = zipfile.ZipInfo(name, date_time=(2026, 11, 18, 9, 0, 0))
        info.compress_type = zipfile.ZIP_DEFLATED
        z.writestr(info, data)
print("ok")
