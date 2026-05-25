/**
 * Heuristics for deciding when a fetched page is too thin to be useful and
 * the next layer should be tried. The basic test: short markdown + a body
 * dominated by an empty container is almost certainly a JS-rendered SPA
 * shell.
 */

const SPA_SIGNALS = [
  /<div id="root">\s*<\/div>/i,
  /<div id="app">\s*<\/div>/i,
  /<div id="__next">\s*<\/div>/i,
  /window\.__NEXT_DATA__/,
  /__NUXT__/,
  /<noscript>[^<]*JavaScript/i,
];

export interface QualityVerdict {
  ok: boolean;
  reason?: string;
}

export function assessQuality(markdown: string, rawHtml: string, minChars = 400): QualityVerdict {
  const trimmed = markdown.trim();

  if (trimmed.length < minChars) {
    return { ok: false, reason: `markdown too short (${trimmed.length} < ${minChars})` };
  }
  for (const pat of SPA_SIGNALS) {
    if (pat.test(rawHtml)) {
      return { ok: false, reason: `SPA shell signal matched: ${pat}` };
    }
  }
  return { ok: true };
}
