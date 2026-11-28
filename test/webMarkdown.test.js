const test = require('node:test');
const assert = require('node:assert/strict');
const { renderMarkdown, renderPlainText, escapeHtml } = require('../dist/web/markdown.logic');

const ATTACHMENT = '<p class="attachment">Attachment, available in Discord</p>';

test('escapeHtml escapes every HTML-significant character', () => {
  assert.equal(escapeHtml(`<a href="x" onclick='y'>&amp;</a>`),
    '&lt;a href=&quot;x&quot; onclick=&#39;y&#39;&gt;&amp;amp;&lt;/a&gt;');
});

test('renderPlainText escapes and keeps line breaks without markdown', () => {
  assert.equal(renderPlainText('**hi** <b>\nthere'), '**hi** &lt;b&gt;<br>there');
  assert.equal(renderPlainText('a\r\nb'), 'a<br>b');
});

test('paragraphs split on blank lines and keep soft breaks', () => {
  assert.equal(renderMarkdown('one\ntwo\n\nthree'), '<p>one<br>two</p>\n<p>three</p>');
  assert.equal(renderMarkdown(''), '');
  assert.equal(renderMarkdown('  \n\n'), '');
});

test('bold, italic and inline code', () => {
  assert.equal(renderMarkdown('**b** and *i* and `x*y*z`'),
    '<p><strong>b</strong> and <em>i</em> and <code>x*y*z</code></p>');
});

test('unordered and ordered lists', () => {
  assert.equal(renderMarkdown('- a\n- **b**\n\n1. one\n2) two'),
    '<ul><li>a</li><li><strong>b</strong></li></ul>\n<ol><li>one</li><li>two</li></ol>');
  assert.equal(renderMarkdown('intro\n* x\n+ y'), '<p>intro</p>\n<ul><li>x</li><li>y</li></ul>');
});

test('fenced code keeps contents escaped and literal, with an optional language', () => {
  assert.equal(renderMarkdown('```ts\nconst a = **1** < 2;\n```'),
    '<pre><code class="language-ts">const a = **1** &lt; 2;</code></pre>');
  assert.equal(renderMarkdown('```\nplain\n```\nafter'), '<pre><code>plain</code></pre>\n<p>after</p>');
});

test('http(s) links get rel and target; everything else is plain text', () => {
  assert.equal(renderMarkdown('see [docs](https://example.com/a_b*c*?x=1&y=2)'),
    '<p>see <a href="https://example.com/a_b*c*?x=1&amp;y=2" rel="noopener noreferrer" target="_blank">docs</a></p>');
  assert.equal(renderMarkdown('[x](http://example.com)'),
    '<p><a href="http://example.com" rel="noopener noreferrer" target="_blank">x</a></p>');
  assert.equal(renderMarkdown('[x](/relative)'), '<p>[x](/relative)</p>');
  assert.equal(renderMarkdown('[x](mailto:a@example.com)'), '<p>[x](mailto:a@example.com)</p>');
});

test('ATTACH lines render as a Discord pointer and never leak the path', () => {
  assert.equal(renderMarkdown('ATTACH: /home/owner/secret.pdf'), ATTACHMENT);
  assert.equal(renderMarkdown('here:\nATTACH:/tmp/a.png\nbye'), `<p>here:</p>\n${ATTACHMENT}\n<p>bye</p>`);
  assert.equal(renderMarkdown('- item\nATTACH: x'), `<ul><li>item</li></ul>\n${ATTACHMENT}`);
});

// XSS corpus: no output may contain a live tag outside the whitelist, an on* attribute,
// or a non-http(s) href.
const XSS_CORPUS = [
  '<script>alert(1)</script>',
  '<SCRIPT SRC=//evil.example/x.js></SCRIPT>',
  '<img src=x onerror=alert(1)>',
  '<svg/onload=alert(1)>',
  '<a href="javascript:alert(1)">x</a>',
  '[click](javascript:alert(1))',
  '[click](JavaScript:alert(1))',
  '[click](javascript&#58;alert(1))',
  '[click](data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==)',
  '[click](vbscript:msgbox(1))',
  '[click](//evil.example)',
  '[x](https://ok.example"onmouseover="alert(1))',
  '[x](https://ok.example" onmouseover="alert(1))',
  '[<img src=x onerror=alert(1)>](https://ok.example)',
  '**<b onmouseover=alert(1)>bold</b>**',
  '`<script>alert(1)</script>`',
  '```\n<script>alert(1)</script>\n```',
  '```"><script>alert(1)</script>\nx\n```',
  '```js\n```html\n<script>alert(1)</script>\n```\n<img src=x onerror=alert(1)>\n```',
  '```\nunterminated <script>alert(1)</script>',
  '&lt;script&gt;alert(1)&lt;/script&gt;',
  '&#60;script&#62;alert(1)&#60;/script&#62;',
  '\u0000<script>alert(1)</script>\u0000',
  'ATTACH: <script>alert(1)</script>',
  'ATTACH: javascript:alert(1)',
  '- <iframe src="javascript:alert(1)"></iframe>',
  '1. <style>body{display:none}</style>',
  '<div style="background:url(javascript:alert(1))">',
  '"><svg onload=alert(1)>',
];
const ALLOWED_TAG = /^<\/?(p|br|strong|em|code|pre|ul|ol|li|a)(\s|>|$)/;

for (const input of XSS_CORPUS) {
  test(`XSS corpus: ${JSON.stringify(input).slice(0, 60)}`, () => {
    const html = renderMarkdown(input);
    for (const tag of html.match(/<[^>]*>?/g) ?? []) {
      assert.match(tag, ALLOWED_TAG, `unexpected tag ${tag} in ${html}`);
      assert.doesNotMatch(tag, /\son\w+\s*=/i, `event handler in ${tag}`);
      assert.doesNotMatch(tag, /style\s*=/i, `style attribute in ${tag}`);
      const attrs = tag.replace(/^<\/?\w+/, '').replace(/>$/, '').trim();
      if (tag.startsWith('<a ')) {
        assert.match(attrs, /^href="https?:\/\/[^"<>\s]*" rel="noopener noreferrer" target="_blank"$/, tag);
      } else if (tag.startsWith('<code ')) {
        assert.match(attrs, /^class="language-[\w+-]+"$/, tag);
      } else if (tag.startsWith('<p ')) {
        assert.equal(attrs, 'class="attachment"');
      } else {
        assert.equal(attrs, '', `unexpected attributes in ${tag}`);
      }
    }
    assert.doesNotMatch(html, /\u0000/);
  });
}

test('XSS corpus specifics: fences, entities and attach payloads stay inert', () => {
  assert.equal(renderMarkdown('```"><script>alert(1)</script>\nx\n```'),
    '<p>```&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;<br>x</p>\n<pre><code></code></pre>');
  assert.equal(renderMarkdown('```js\n```html\n<b>\n```\nafter'),
    '<pre><code class="language-js">```html\n&lt;b&gt;</code></pre>\n<p>after</p>');
  assert.equal(renderMarkdown('```\nopen <i>\nATTACH: x'),
    '<pre><code>open &lt;i&gt;\nATTACH: x</code></pre>');
  assert.equal(renderMarkdown('&lt;script&gt;'), '<p>&amp;lt;script&amp;gt;</p>');
  assert.equal(renderMarkdown('ATTACH: <script>alert(1)</script>'), ATTACHMENT);
  assert.equal(renderMarkdown('[x](javascript:alert(1))'), '<p>[x](javascript:alert(1))</p>');
});
