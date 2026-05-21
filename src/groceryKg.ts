/**
 * Read/write helpers for the grocery knowledge graph under wiki/groceries/.
 * Path-restricted: requests outside wiki/groceries/ are rejected so the agent
 * tool can't accidentally clobber other wiki pages.
 */

import fs from "fs";
import path from "path";

const KG_ROOT = path.join(process.cwd(), "wiki", "groceries");

function resolveSafe(rel: string): string {
  const cleaned = rel.replace(/^\/+/, "");
  const abs = path.resolve(KG_ROOT, cleaned);
  const rootResolved = path.resolve(KG_ROOT);
  if (abs !== rootResolved && !abs.startsWith(rootResolved + path.sep)) {
    throw new Error(`grocery KG path escapes wiki/groceries/: ${rel}`);
  }
  if (!abs.endsWith(".md") && !abs.endsWith(".markdown")) {
    throw new Error(`grocery KG path must end with .md: ${rel}`);
  }
  return abs;
}

export function kgList(): string[] {
  if (!fs.existsSync(KG_ROOT)) return [];
  const walk = (dir: string, prefix: string): string[] => {
    const out: string[] = [];
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const sub = path.join(dir, entry.name);
      const rel = prefix ? `${prefix}/${entry.name}` : entry.name;
      if (entry.isDirectory()) out.push(...walk(sub, rel));
      else if (entry.name.endsWith(".md")) out.push(rel);
    }
    return out;
  };
  return walk(KG_ROOT, "").sort();
}

export function kgRead(rel: string): string {
  const abs = resolveSafe(rel);
  if (!fs.existsSync(abs)) throw new Error(`grocery KG page not found: ${rel}`);
  return fs.readFileSync(abs, "utf-8");
}

export function kgWrite(rel: string, content: string): void {
  const abs = resolveSafe(rel);
  fs.mkdirSync(path.dirname(abs), { recursive: true });
  fs.writeFileSync(abs, content, "utf-8");
}

export function kgAppend(rel: string, content: string): void {
  const abs = resolveSafe(rel);
  fs.mkdirSync(path.dirname(abs), { recursive: true });
  fs.appendFileSync(abs, content, "utf-8");
}

export interface OrderRecord {
  items_ordered: Array<{ prodId?: string | number; name: string; brand?: string; qty: number; price?: number }>;
  items_oos?: Array<{ name: string; reason?: string }>;
  substitutions?: Array<{ from: string; to: string }>;
  total?: number;
  feedback?: string;
  approved?: boolean;
}

export function recordOrder(date: string, order: OrderRecord): string {
  const safeDate = date.replace(/[^0-9-]/g, "");
  const rel = `orders/${safeDate || new Date().toISOString().slice(0, 10)}.md`;
  const lines: string[] = [];
  lines.push("---");
  lines.push(`type: grocery-order`);
  lines.push(`date: ${safeDate || new Date().toISOString().slice(0, 10)}`);
  lines.push(`approved: ${order.approved ?? false}`);
  if (order.total != null) lines.push(`total: ${order.total}`);
  lines.push("---");
  lines.push("");
  lines.push("## Items ordered");
  for (const it of order.items_ordered) {
    const parts = [`${it.qty}× ${it.name}`];
    if (it.brand) parts.push(`(${it.brand})`);
    if (it.price != null) parts.push(`— $${it.price.toFixed(2)}`);
    if (it.prodId != null) parts.push(`[prodId: ${it.prodId}]`);
    lines.push(`- ${parts.join(" ")}`);
  }
  if (order.items_oos?.length) {
    lines.push("");
    lines.push("## Out of stock");
    for (const it of order.items_oos) {
      lines.push(`- ${it.name}${it.reason ? ` — ${it.reason}` : ""}`);
    }
  }
  if (order.substitutions?.length) {
    lines.push("");
    lines.push("## Substitutions");
    for (const s of order.substitutions) lines.push(`- ${s.from} → ${s.to}`);
  }
  if (order.feedback) {
    lines.push("");
    lines.push("## Feedback");
    lines.push(order.feedback);
  }
  lines.push("");
  kgWrite(rel, lines.join("\n"));
  return rel;
}
