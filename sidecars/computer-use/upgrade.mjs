import { readFile, writeFile, rename } from "node:fs/promises";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { refreshModel, GATES, recommendedCandidate } from "./models.mjs";
import { runFixture } from "./fixture.mjs";
const here = dirname(fileURLToPath(import.meta.url));
const exec = promisify(execFile);
export async function loadPolicy(directory) {
  try {
    return JSON.parse(
      await readFile(join(directory, "model-policy.json"), "utf8"),
    );
  } catch (e) {
    if (e.code !== "ENOENT") throw e;
    return { mode: "auto-validated", current: "gpt-6-astra", previous: null };
  }
}
export async function checkUpgrade(directory) {
  const state = await loadPolicy(directory);
  if (
    Date.now() - Date.parse(state.checkedAt ?? state.validatedAt ?? "") <
    86400000
  )
    return state;
  const source = "https://developers.openai.com/api/docs/guides/latest-model";
  let catalog;
  const readCatalog = async () => {
    if (catalog) return catalog;
    const cache = JSON.parse(
      await readFile(
        join(process.env.HOME, ".codex/models_cache.json"),
        "utf8",
      ),
    );
    if (
      !Number.isFinite(Date.parse(cache.fetched_at)) ||
      Date.now() - Date.parse(cache.fetched_at) > 86400000
    )
      throw Error("model_catalog_stale");
    catalog = cache.models;
    return catalog;
  };
  const result = await refreshModel(state, {
    revision: (
      await exec("git", ["rev-parse", "HEAD"], { cwd: here })
    ).stdout.trim(),
    discover: async () => {
      const response = await fetch(source, {
        signal: AbortSignal.timeout(10000),
      });
      if (
        !response.ok ||
        new URL(response.url).hostname !== "developers.openai.com"
      )
        throw Error("discovery_unavailable");
      const text = (await response.text())
        .replace(/&quot;|&#34;/g, '"')
        .replace(/&#39;|&#x27;/g, "'")
        .replace(/<[^>]*>/g, "");
      const model = recommendedCandidate(await readCatalog(), text);
      return { model, source };
    },
    available: async () =>
      (await readCatalog())
        .filter((m) => m.visibility === "list")
        .map((m) => m.slug),
    evaluate: async (model) => {
      await exec(
        process.execPath,
        [
          "--test",
          "test/tasks.test.mjs",
          "test/network.test.mjs",
          "test/browser.test.mjs",
          "test/runner.test.mjs",
          "test/evidence.test.mjs",
          "test/service.test.mjs",
        ],
        { cwd: here, timeout: 60000 },
      );
      const result = await runFixture(model);
      if (
        !/187/.test(result.answer) ||
        !/2026-09-24|September 24/.test(result.answer) ||
        !/Founder|19:00|7[ :]*[pP]/.test(result.answer) ||
        result.unresolved?.length
      )
        throw Error("candidate_fixture_failed");
      return Object.fromEntries(GATES.map((g) => [g, true]));
    },
  });
  const path = join(directory, "model-policy.json");
  await writeFile(path + ".tmp", JSON.stringify(result), { mode: 0o600 });
  await rename(path + ".tmp", path);
  return result;
}
