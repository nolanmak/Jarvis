// #1411 — Remotion writes its webpack bundle to a fresh
// `remotion-webpack-bundle-*` dir in the system temp dir on every start and
// never removes it. A clean shutdown removes this process's own; this module
// decides which dirs left by processes that were killed are safe to take.

export const BUNDLE_PREFIX = 'remotion-webpack-bundle-';
export const OWNER_FILE = '.owner-pid';
/** A bundle with no owner marker (written before this module existed). */
export const UNMARKED_MAX_AGE_MS = 7 * 24 * 60 * 60 * 1000;

/**
 * Names of bundle dirs to remove.
 *
 * @param {{name: string, ownerPid: number|null, ageMs: number}[]} dirs
 * @param {(pid: number) => boolean} isAlive
 * @param {number} selfPid
 */
export function staleBundles(dirs, isAlive, selfPid) {
  return dirs
    .filter((d) => d.name.startsWith(BUNDLE_PREFIX))
    .filter((d) => {
      if (d.ownerPid === selfPid) return false;
      // Marked: gone exactly when its owner is. Another live renderer (a
      // second tenant) keeps serving from its bundle for as long as it runs.
      if (Number.isInteger(d.ownerPid)) return !isAlive(d.ownerPid);
      return d.ageMs > UNMARKED_MAX_AGE_MS;
    })
    .map((d) => d.name);
}
