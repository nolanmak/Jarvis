// Server-side markdown for Jarvis Dashboard answers (spec §5.4).
// Everything is HTML-escaped first; only a small whitelist of markup is then produced
// from the escaped text, so no input character can open a tag or an attribute.

const ESCAPES: Record<string, string> = {
  '&': '&amp;',
  '<': '&lt;',
  '>': '&gt;',
  '"': '&quot;',
  "'": '&#39;',
};

const ATTACHMENT_HTML = '<p class="attachment">Attachment, available in Discord</p>';
const FENCE_OPEN = /^```([\w+-]*)\s*$/;
const FENCE_CLOSE = /^```\s*$/;
const UNORDERED_ITEM = /^\s*[-*+]\s+(.*)$/;
const ORDERED_ITEM = /^\s*\d+[.)]\s+(.*)$/;
const SAFE_URL = /^https?:\/\/\S+$/i;
// Placeholder delimiter; stripped from input so it cannot be forged.
const MARK = '\u0000';

export function escapeHtml(text: string): string {
  return text.replace(/[&<>"']/g, (ch) => ESCAPES[ch]);
}

/** Escaped plain text with line breaks kept; used for approval previews. */
export function renderPlainText(text: string): string {
  return escapeHtml(normalize(text)).replace(/\n/g, '<br>');
}

export function renderMarkdown(text: string): string {
  const lines = escapeHtml(normalize(text)).split('\n');
  const blocks: string[] = [];
  let paragraph: string[] = [];
  let list: { tag: 'ul' | 'ol'; items: string[] } | null = null;

  const flushParagraph = () => {
    if (paragraph.length) blocks.push(`<p>${paragraph.map(renderInline).join('<br>')}</p>`);
    paragraph = [];
  };
  const flushList = () => {
    if (list) blocks.push(`<${list.tag}>${list.items.map((i) => `<li>${renderInline(i)}</li>`).join('')}</${list.tag}>`);
    list = null;
  };
  const flush = () => {
    flushParagraph();
    flushList();
  };

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const fence = FENCE_OPEN.exec(line);
    if (fence) {
      flush();
      const body: string[] = [];
      // Only a bare ``` closes; an unterminated fence runs to the end of the text.
      for (i++; i < lines.length && !FENCE_CLOSE.test(lines[i]); i++) body.push(lines[i]);
      const cls = fence[1] ? ` class="language-${fence[1]}"` : '';
      blocks.push(`<pre><code${cls}>${body.join('\n')}</code></pre>`);
      continue;
    }
    if (line.startsWith('ATTACH:')) {
      flush();
      blocks.push(ATTACHMENT_HTML);
      continue;
    }
    if (!line.trim()) {
      flush();
      continue;
    }
    const unordered = UNORDERED_ITEM.exec(line);
    const ordered = unordered ? null : ORDERED_ITEM.exec(line);
    if (unordered || ordered) {
      flushParagraph();
      const tag = unordered ? 'ul' : 'ol';
      if (list && list.tag !== tag) flushList();
      list ??= { tag, items: [] };
      list.items.push((unordered ?? ordered)![1]);
      continue;
    }
    flushList();
    paragraph.push(line);
  }
  flush();
  return blocks.join('\n');
}

function normalize(text: string): string {
  return text.replace(/\r\n?/g, '\n').split(MARK).join('');
}

/** Inline markup over already-escaped text: code spans, http(s) links, bold, italic. */
function renderInline(escaped: string): string {
  const tokens: string[] = [];
  const hold = (html: string) => `${MARK}${tokens.push(html) - 1}${MARK}`;
  let html = emphasis(
    escaped
      .replace(/`([^`]+)`/g, (_m, code: string) => hold(`<code>${code}</code>`))
      .replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (match, label: string, url: string) =>
        SAFE_URL.test(url)
          ? hold(`<a href="${url}" rel="noopener noreferrer" target="_blank">${emphasis(label)}</a>`)
          : match,
      ),
  );
  // A link label may hold a code-span token, so restore until none remain.
  const token = new RegExp(`${MARK}(\\d+)${MARK}`, 'g');
  while (html.includes(MARK)) html = html.replace(token, (_m, n: string) => tokens[Number(n)]);
  return html;
}

function emphasis(text: string): string {
  return text
    .replace(/\*\*([^*]+?)\*\*/g, '<strong>$1</strong>')
    .replace(/\*([^*]+?)\*/g, '<em>$1</em>');
}
