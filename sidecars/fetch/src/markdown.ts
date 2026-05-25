import TurndownService from "turndown";

let td: TurndownService | null = null;

function instance(): TurndownService {
  if (td) return td;
  td = new TurndownService({
    headingStyle: "atx",
    codeBlockStyle: "fenced",
    bulletListMarker: "-",
    emDelimiter: "_",
  });
  td.remove(["script", "style", "noscript", "iframe"]);
  td.addRule("strip-svg", {
    filter: (node) => node.nodeName === "SVG",
    replacement: () => "",
  });
  return td;
}

export function htmlToMarkdown(html: string): string {
  if (!html) return "";
  try {
    return instance().turndown(html).trim();
  } catch {
    return stripTags(html);
  }
}

export function stripTags(html: string): string {
  return html
    .replace(/<script[\s\S]*?<\/script>/gi, "")
    .replace(/<style[\s\S]*?<\/style>/gi, "")
    .replace(/<[^>]+>/g, " ")
    .replace(/\s+/g, " ")
    .trim();
}

export function extractTitle(html: string): string | undefined {
  const m = /<title[^>]*>([^<]*)<\/title>/i.exec(html);
  return m ? m[1].trim() : undefined;
}
