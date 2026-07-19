// --- Auto-update signature gate (security #298) ---------------------------
// This host holds live session credentials. Both auto-update paths (the 5-min
// poller in index.ts and the GitHub webhook receiver in dashboard.ts) pull +
// build + restart whatever lands on `main`. The git *author* field is
// spoofable, so we must NOT trust it. Instead we require the target revision to
// carry a valid cryptographic signature from an allowlisted (owner) key before
// we ever pull, install, build, or restart.
//
// This module is the single source of truth for that gate so both callers
// behave identically.
//
// Env knobs:
//   AUGMENTAGENT_UPDATE_ALLOWED_SIGNERS  Path to an SSH/GPG allowed-signers file
//                                        (the trust anchor). Required when
//                                        signature verification is enforced and
//                                        no allowedSignersFile is already wired
//                                        into git config.
//   AUGMENTAGENT_UPDATE_REQUIRE_SIGNATURE  "false" disables the gate (escape
//                                        hatch for users who cannot sign).
//                                        Default = enforce (any other value /
//                                        unset = require signature).
import { execSync } from "child_process";

export const UPDATE_REQUIRE_SIGNATURE =
  (process.env.AUGMENTAGENT_UPDATE_REQUIRE_SIGNATURE || "").trim().toLowerCase() !==
  "false";
export const UPDATE_ALLOWED_SIGNERS = (
  process.env.AUGMENTAGENT_UPDATE_ALLOWED_SIGNERS || ""
).trim();

/**
 * Best-effort Discord alert for security-relevant auto-update events.
 * Uses the simple webhook (no bot required) so it works even before/without the
 * Discord bot being ready. Never throws — alerting must not break the caller.
 */
export function alertUpdateSecurity(message: string): void {
  const webhook = (process.env.DISCORD_WEBHOOK_URL || "").trim();
  if (!webhook) return;
  // fire-and-forget; swallow all errors so a flaky webhook can't crash the loop
  void fetch(webhook, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ content: `⚠️ AugmentAgent auto-update: ${message}` }),
  }).catch(() => {
    /* alerting is best-effort */
  });
}

/**
 * Verify that `sha` (origin/main HEAD) is cryptographically signed by an
 * allowlisted key. Returns true only when git confirms a GOOD signature from a
 * trusted signer; returns false for unsigned, bad, or untrusted-signer commits.
 *
 * Trust anchor: an allowed-signers file. We pass it explicitly via
 * AUGMENTAGENT_UPDATE_ALLOWED_SIGNERS when provided so the owner's key is the
 * only thing that can authorize a deploy; otherwise we rely on git's already
 * configured gpg.ssh.allowedSignersFile / GPG keyring. The git *author* field is
 * never consulted — only the signature.
 */
export function isRevisionSignedByOwner(cwd: string, sha: string): boolean {
  // Build a `-c` override so the allowed-signers file is the trust anchor for
  // this invocation only (does not mutate repo/global git config).
  const signersOverride = UPDATE_ALLOWED_SIGNERS
    ? `-c gpg.ssh.allowedSignersFile=${JSON.stringify(UPDATE_ALLOWED_SIGNERS)} `
    : "";

  // Prefer a signed release tag pointing at this revision (decouples "deploy"
  // from "every push to main"); fall back to a signed commit on the revision.
  const candidates = [
    // signed tag exactly at origin/main HEAD, verified against allowed signers
    `git ${signersOverride}tag --points-at ${sha} --format='%(refname:short)'`,
  ];

  // 1) If a tag points at this sha, require git verify-tag to pass for it.
  try {
    const tags = execSync(candidates[0]!, { cwd, stdio: "pipe" })
      .toString()
      .split("\n")
      .map((t) => t.trim())
      .filter(Boolean);
    for (const tag of tags) {
      try {
        execSync(`git ${signersOverride}verify-tag ${JSON.stringify(tag)}`, {
          cwd,
          stdio: "pipe",
        });
        console.log(
          `[${new Date().toISOString()}] Signature OK: tag ${tag} -> ${sha.slice(0, 7)} verified.`
        );
        return true;
      } catch {
        // this tag failed verification; keep checking others / fall through
      }
    }
  } catch {
    // tag enumeration failed; fall through to commit verification
  }

  // 2) Otherwise require the commit itself to be signed by an allowed key.
  try {
    execSync(`git ${signersOverride}verify-commit ${sha}`, { cwd, stdio: "pipe" });
    console.log(
      `[${new Date().toISOString()}] Signature OK: commit ${sha.slice(0, 7)} verified.`
    );
    return true;
  } catch {
    return false;
  }
}
