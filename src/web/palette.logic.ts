// Ranking for the Jarvis Dashboard command palette (spec §5.4). Pure: no I/O, no clock,
// and the inputs are never mutated.

export interface PaletteAction {
  id: string;
  label: string;
  href: string;
}

export interface PaletteThread {
  id: string;
  title: string;
  /** Epoch milliseconds of last activity. */
  updatedAt: number;
}

export type PaletteEntry =
  | { kind: 'action'; id: string; label: string; href: string }
  | { kind: 'thread'; id: string; label: string };

export interface PaletteOptions {
  limit?: number;
}

enum Tier {
  Prefix = 0,
  WordStart = 1,
  Subsequence = 2,
}

interface Candidate {
  entry: PaletteEntry;
  tier: Tier;
  kindRank: number;
  updatedAt: number;
}

export function rankPalette(
  query: string,
  sources: { actions: readonly PaletteAction[]; threads: readonly PaletteThread[] },
  options: PaletteOptions = {},
): PaletteEntry[] {
  const q = query.trim().toLowerCase();
  const candidates: Candidate[] = [];

  sources.actions.forEach((a, index) => {
    const tier = matchTier(q, a.label);
    if (tier !== null) {
      // Actions keep their given order: earlier actions read as more recent.
      candidates.push({ entry: { kind: 'action', id: a.id, label: a.label, href: a.href }, tier, kindRank: 0, updatedAt: -index });
    }
  });
  for (const t of sources.threads) {
    const tier = matchTier(q, t.title);
    if (tier !== null) {
      candidates.push({ entry: { kind: 'thread', id: t.id, label: t.title }, tier, kindRank: 1, updatedAt: t.updatedAt });
    }
  }

  candidates.sort(
    (a, b) =>
      a.tier - b.tier ||
      a.kindRank - b.kindRank ||
      b.updatedAt - a.updatedAt ||
      (a.entry.id < b.entry.id ? -1 : a.entry.id > b.entry.id ? 1 : 0),
  );
  const ranked = candidates.map((c) => c.entry);
  return options.limit === undefined ? ranked : ranked.slice(0, Math.max(0, options.limit));
}

/** Best match tier for `q` (already lower-cased) in `label`, or null. Empty query matches all. */
function matchTier(q: string, label: string): Tier | null {
  const text = label.toLowerCase();
  if (text.startsWith(q)) return Tier.Prefix;
  if (text.split(/[^\p{L}\p{N}]+/u).some((word) => word.startsWith(q))) return Tier.WordStart;
  return isSubsequence(q, text) ? Tier.Subsequence : null;
}

function isSubsequence(q: string, text: string): boolean {
  const want = Array.from(q);
  let i = 0;
  for (const ch of text) {
    if (ch === want[i]) i++;
    if (i === want.length) return true;
  }
  return false;
}
