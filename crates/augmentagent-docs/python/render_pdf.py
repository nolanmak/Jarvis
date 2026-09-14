"""Local Markdown -> PDF worker, embedded in the Rust binary (#992).

Input: UTF-8 Markdown on stdin. Output: PDF bytes on stdout. No resource loads
from document content: raw HTML is literal, images become alt text, and links
are printed as citations. Only the installed Liberation font files are opened.
"""
import io
import sys
from html import escape
from html.parser import HTMLParser
from pathlib import Path
from xml.etree.ElementTree import Element, SubElement

import markdown
from reportlab.lib import colors
from reportlab.lib.enums import TA_LEFT
from reportlab.lib.pagesizes import letter
from reportlab.lib.styles import ParagraphStyle, getSampleStyleSheet
from reportlab.pdfbase import pdfmetrics
from reportlab.pdfbase.ttfonts import TTFont
from reportlab.platypus import (
    HRFlowable, LongTable, Paragraph, SimpleDocTemplate, Spacer, TableStyle,
)


class TreeParser(HTMLParser):
    def __init__(self, html):
        super().__init__(convert_charrefs=True)
        self.root = Element('document')
        self.stack = [self.root]
        self.feed(html)
        self.close()

    def handle_starttag(self, tag, attrs):
        node = SubElement(self.stack[-1], tag, dict(attrs))
        if tag not in ('img', 'br', 'hr'):
            self.stack.append(node)

    def handle_startendtag(self, tag, attrs):
        self.handle_starttag(tag, attrs)
        if tag not in ('img', 'br', 'hr'):
            self.handle_endtag(tag)

    def handle_endtag(self, tag):
        if len(self.stack) > 1 and self.stack[-1].tag == tag:
            self.stack.pop()

    def handle_data(self, data):
        parent = self.stack[-1]
        if len(parent):
            parent[-1].tail = (parent[-1].tail or '') + data
        else:
            parent.text = (parent.text or '') + data


def inline(node):
    result = escape(node.text or '')
    for child in node:
        body = inline(child)
        if child.tag in ('strong', 'b'):
            result += '<b>' + body + '</b>'
        elif child.tag in ('em', 'i'):
            result += '<i>' + body + '</i>'
        elif child.tag == 'br':
            result += '<br/>'
        elif child.tag == 'img':
            result += '[Image: ' + escape(child.get('alt') or 'image') + ']'
        elif child.tag == 'a':
            href = child.get('href', '')
            result += body
            if href and href != ''.join(child.itertext()):
                result += ' (' + escape(href) + ')'
        else:
            result += body
        result += escape(child.tail or '')
    return result


def render_pdf(source):
    if not source.strip():
        raise ValueError('cannot render an empty document')
    font_dir = Path('/usr/share/fonts/truetype/liberation')
    for name, file in [('Doc', 'LiberationSans-Regular.ttf'), ('Doc-Bold', 'LiberationSans-Bold.ttf'),
                       ('Doc-Italic', 'LiberationSans-Italic.ttf'), ('Doc-BoldItalic', 'LiberationSans-BoldItalic.ttf')]:
        if not (font_dir / file).is_file():
            raise RuntimeError('PDF fonts missing: install fonts-liberation')
        if name not in pdfmetrics.getRegisteredFontNames():
            pdfmetrics.registerFont(TTFont(name, str(font_dir / file)))
    pdfmetrics.registerFontFamily('Doc', normal='Doc', bold='Doc-Bold', italic='Doc-Italic', boldItalic='Doc-BoldItalic')
    styles = getSampleStyleSheet()
    for style in styles.byName.values():
        style.fontName = 'Doc'
    body = ParagraphStyle('DocumentBody', parent=styles['BodyText'], fontSize=10, leading=15, spaceAfter=8, alignment=TA_LEFT)
    code = ParagraphStyle('DocumentCode', parent=body, fontSize=9, leading=12, backColor=colors.HexColor('#f3f4f6'))
    md = markdown.Markdown(extensions=['tables', 'fenced_code', 'sane_lists'])
    # Disable raw HTML passthrough before converting Markdown to our small,
    # explicitly supported presentation vocabulary.
    md.preprocessors.deregister('html_block')
    md.inlinePatterns.deregister('html')
    root = TreeParser(md.convert(source)).root
    story = []
    width = letter[0] - 108 - 12  # default frame padding

    def paragraph(text, style=body):
        return Paragraph(text or '&#160;', style)

    def blocks(node, depth=0):
        if depth > 20:
            raise ValueError('document nesting exceeds 20 levels')
        for child in node:
            tag = child.tag
            if tag in ('ul', 'ol'):
                for number, item in enumerate(child, 1):
                    prefix = f'{number}.' if tag == 'ol' else '•'
                    style = ParagraphStyle(f'List{depth}', parent=body, leftIndent=16 * (depth + 1))
                    # Nested lists are separate flowables so long lists paginate.
                    direct = Element('span')
                    direct.text = item.text
                    for part in item:
                        if part.tag not in ('ul', 'ol'):
                            direct.append(part)
                    story.append(paragraph(escape(prefix) + ' ' + inline(direct), style))
                    for part in item:
                        if part.tag in ('ul', 'ol'):
                            wrapper = Element('div')
                            wrapper.append(part)
                            blocks(wrapper, depth + 1)
            elif tag == 'table':
                rows = [[paragraph(inline(cell)) for cell in row] for row in child.iter('tr')]
                if rows:
                    count = max(map(len, rows))
                    rows = [row + [''] * (count - len(row)) for row in rows]
                    table = LongTable(rows, colWidths=[width / count] * count, repeatRows=1, splitInRow=1, hAlign='LEFT')
                    table.setStyle(TableStyle([
                        ('BACKGROUND', (0, 0), (-1, 0), colors.HexColor('#e5e7eb')),
                        ('GRID', (0, 0), (-1, -1), 0.4, colors.HexColor('#9ca3af')),
                        ('VALIGN', (0, 0), (-1, -1), 'TOP'),
                        ('LEFTPADDING', (0, 0), (-1, -1), 6),
                        ('RIGHTPADDING', (0, 0), (-1, -1), 6),
                    ]))
                    story.extend([table, Spacer(1, 10)])
            elif tag == 'pre':
                # One flowable per line allows arbitrarily long code blocks to
                # paginate; Paragraph also wraps lines wider than the page.
                for line in ''.join(child.itertext()).splitlines():
                    story.append(paragraph(escape(line).replace(' ', '&#160;'), code))
            elif tag == 'hr':
                story.append(HRFlowable(width='100%', spaceBefore=8, spaceAfter=8))
            elif tag == 'blockquote':
                blocks(child, depth + 1)
            elif tag in ('h1', 'h2', 'h3', 'h4', 'h5', 'h6'):
                story.append(paragraph(inline(child), styles['Heading' + tag[1]]))
            else:
                story.append(paragraph(inline(child)))

    blocks(root)
    if not story:
        raise ValueError('cannot render an empty document')
    output = io.BytesIO()
    title = next((''.join(n.itertext()) for n in root if n.tag == 'h1'), 'Document')
    doc = SimpleDocTemplate(output, pagesize=letter, rightMargin=54, leftMargin=54,
                            topMargin=54, bottomMargin=54, title=title)

    def footer(canvas, document):
        canvas.saveState()
        canvas.setFont('Doc', 8)
        canvas.drawRightString(letter[0] - 54, 30, f'Page {document.page}')
        canvas.restoreState()

    doc.build(story, onFirstPage=footer, onLaterPages=footer)
    return output.getvalue()


if __name__ == '__main__':
    try:
        sys.stdout.buffer.write(render_pdf(sys.stdin.read()))
    except Exception as error:
        print(f'PDF render failed: {error}', file=sys.stderr)
        sys.exit(1)
