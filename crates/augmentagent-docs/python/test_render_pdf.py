import subprocess
import unittest
from unittest.mock import patch

from render_pdf import render_pdf


def extracted(pdf):
    return subprocess.run(['pdftotext', '-', '-'], input=pdf, capture_output=True, check=True).stdout.decode()


class RenderPdfTests(unittest.TestCase):
    def test_document_formatting_and_unicode(self):
        pdf = render_pdf('# Lawyer packet\n\nCafé — “quoted” & <literal>.\n\n## Evidence\n\n- First **important** item\n- Second item\n\n1. Ordered step\n2. Next step\n\n| Date | Event |\n| --- | --- |\n| September | Inspection |\n\n```text\ncode <sample>\n```\n\n[Source](https://example.com/source)')
        self.assertTrue(pdf.startswith(b'%PDF-'))
        text = extracted(pdf)
        for expected in ['Lawyer packet', 'Café', 'quoted', 'Evidence', 'important', 'Ordered step', 'September', 'Inspection', 'code <sample>', 'https://example.com/source']:
            self.assertIn(expected, text)

    def test_long_document_paginates_without_losing_tail(self):
        text = extracted(render_pdf('# Long report\n\n' + '\n\n'.join(f'Paragraph {i}: ' + 'Evidence and detail. ' * 40 for i in range(80)) + '\n\nEND OF REPORT'))
        self.assertGreater(text.count('\f'), 2)
        self.assertIn('END OF REPORT', text)

    def test_images_and_raw_html_never_load_resources(self):
        with patch('urllib.request.urlopen', side_effect=AssertionError('no network')):
            text = extracted(render_pdf('![Evidence photo](file:///etc/passwd)\n\n<img src="https://example.com/private.png">\n\n<script>alert(1)</script>'))
        self.assertIn('Evidence photo', text)
        self.assertNotIn('root:x:', text)

    def test_long_table_splits_across_pages(self):
        md = '| Item | Notes |\n| --- | --- |\n' + '\n'.join(f'| Row {i} | ' + 'Detail ' * 20 + ' |' for i in range(150))
        text = extracted(render_pdf(md))
        self.assertIn('Row 149', text)
        self.assertGreater(text.count('\f'), 2)

    def test_table_row_taller_than_a_page_keeps_all_text(self):
        md = '| Evidence |\n| --- |\n| ' + 'Long detail. ' * 2000 + 'FINAL CELL |'
        self.assertIn('FINAL CELL', extracted(render_pdf(md)))

    def test_empty_document_is_rejected(self):
        with self.assertRaisesRegex(ValueError, 'empty'):
            render_pdf('  \n')


if __name__ == '__main__':
    unittest.main()
