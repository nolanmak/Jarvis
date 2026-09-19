import http from "node:http";
import {
  mkdirSync,
  readFileSync,
  writeFileSync,
  chmodSync,
  existsSync,
  unlinkSync,
} from "node:fs";
import { join } from "node:path";
import { randomBytes, timingSafeEqual, createHash } from "node:crypto";
import { Tasks } from "./tasks.mjs";
import { BrowserSession } from "./browser.mjs";
import { runModel } from "./runner.mjs";
import { loadPolicy, checkUpgrade } from "./upgrade.mjs";
import { validateEvidence, redactObservation } from "./evidence.mjs";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const stateDirectory = process.env.JARVIS_COMPUTER_STATE;
const socket = process.env.JARVIS_COMPUTER_SOCKET;
const profileDirectory = process.env.JARVIS_COMPUTER_CHROME_BRIDGE;
const lockDirectory = process.env.JARVIS_COMPUTER_LOCK_DIRECTORY;
if (
  ![stateDirectory, socket, profileDirectory, lockDirectory].every((p) =>
    p?.startsWith("/"),
  )
)
  throw Error("absolute_configuration_paths_required");
process.umask(0o077);
mkdirSync(stateDirectory, { recursive: true, mode: 0o700 });
// Kernel lease survives neither crashes nor reboot, unlike a PID file. Only
// its holder may recover tasks or replace the Unix socket.
if (process.env.JARVIS_COMPUTER_LOCKED !== "1") {
  const child = spawn(
    "flock",
    [
      "--nonblock",
      join(stateDirectory, "worker.lock"),
      process.execPath,
      fileURLToPath(import.meta.url),
    ],
    {
      env: { ...process.env, JARVIS_COMPUTER_LOCKED: "1" },
      stdio: "inherit",
      detached: true,
    },
  );
  for (const signal of ["SIGTERM", "SIGINT"])
    process.on(signal, () => {
      try {
        process.kill(-child.pid, signal);
      } catch {}
    });
  child.on("error", () => process.exit(1));
  child.on("exit", (code) => process.exit(code ?? 1));
  await new Promise(() => {});
}
const tokenFile = join(stateDirectory, "token");
if (!existsSync(tokenFile))
  writeFileSync(tokenFile, randomBytes(32).toString("hex"), { mode: 0o600 });
const master = readFileSync(tokenFile, "utf8").trim();
const tasks = new Tasks(stateDirectory, await loadPolicy(stateDirectory));
let active = null;
function authorized(a, b) {
  return (
    typeof a === "string" &&
    a.length === b.length &&
    timingSafeEqual(Buffer.from(a), Buffer.from(b))
  );
}
function report(t) {
  const { inputHash, resumeEvents, owner, request, ...result } = t;
  if (["queued", "running"].includes(t.status)) {
    delete result.evidence;
    result.progress = {
      actions: t.actions,
      instruction:
        "Still running. Poll status; do not answer from intermediate observations.",
    };
    return result;
  }
  if (t.flight && t.result)
    result.deliveryInstruction =
      "Return result.answer verbatim as the flight findings. Do not reclassify fare brands or add baggage claims. Economy including Basic is a search filter, not proof of a Basic fare.";
  const cited = new Set(t.result?.evidenceIds ?? []);
  result.evidence = t.evidence
    .filter((e) => (cited.size ? cited.has(e.id) : true))
    .slice(-10)
    .map((e) => ({
      id: e.id,
      url: e.url,
      title: e.title,
      observedAt: e.observedAt,
      screenshotSha256: e.screenshotSha256,
      excerpt: t.result ? undefined : e.text.slice(0, 1000),
    }));
  return result;
}
async function execute(t) {
  const generation = tasks.claim(t.id);
  const controller = new AbortController();
  const token = randomBytes(32).toString("hex");
  const started = Date.now();
  const session = new BrowserSession({ profileDirectory, lockDirectory });
  active = {
    id: t.id,
    owner: t.owner,
    generation,
    controller,
    token,
    session,
    busy: false,
  };
  const current = active;
  const timeout = setTimeout(
    () => {
      current.reason = "time_budget_exhausted";
      controller.abort();
      void session.close().catch(() => {});
    },
    Math.max(1, 300000 - t.elapsedMs),
  );
  let result, reason, usage;
  try {
    await session.open(t.hosts);
    const r = await runModel(
      t,
      { stateDirectory, socket, token },
      controller.signal,
    );
    usage = r.usage;
    result = current.result;
    if (!result) reason = current.reason ?? "model_finished_without_evidence";
  } catch (e) {
    reason = e.message;
  } finally {
    clearTimeout(timeout);
    try {
      await session.close();
    } catch {
      reason = "cleanup_required";
    }
    try {
      tasks.update(t.id, generation, {
        elapsedMs: t.elapsedMs + Date.now() - started,
        usage,
      });
      if (result && !reason) tasks.finish(t.id, generation, result);
      else
        tasks.update(t.id, generation, {
          status:
            reason === "time_budget_exhausted" ? "partial" : "needs_action",
          reason,
        });
    } catch {
      /* cancellation fenced the worker */
    }
    if (active === current) active = null;
  }
}
const server = http.createServer(async (req, res) => {
  try {
    if (req.method !== "POST" || req.url !== "/")
      throw Error("invalid_request");
    let raw = "";
    for await (const chunk of req) {
      raw += chunk;
      if (raw.length > 24000) throw Error("request_too_large");
    }
    const body = JSON.parse(raw),
      token = req.headers.authorization?.replace(/^Bearer /, "");
    let value;
    if (authorized(token, master)) {
      if (
        typeof body.owner !== "string" ||
        !body.owner ||
        typeof body.request !== "string" ||
        !body.request
      )
        throw Error("unauthorized");
      switch (body.operation) {
        case "start":
          value = tasks.start(body.owner, body.request, {
            goal: body.goal,
            hosts: body.hosts,
            ...(body.flight ? { flight: body.flight } : {}),
          });
          break;
        case "status": {
          const deadline = Date.now() + 20000;
          value = tasks.get(body.owner, body.taskId);
          while (
            ["queued", "running"].includes(value.status) &&
            Date.now() < deadline
          ) {
            await new Promise((r) => setTimeout(r, 500));
            value = tasks.get(body.owner, body.taskId);
          }
          break;
        }
        case "cancel":
          value = tasks.cancel(body.owner, body.taskId);
          if (active?.id === body.taskId) {
            active.controller.abort();
            await active.session.close();
          }
          break;
        case "resume":
          value = tasks.resume(body.owner, body.taskId, body.request);
          break;
        default:
          throw Error("invalid_operation");
      }
      value = report(value);
    } else if (active && authorized(token, active.token)) {
      const current = active;
      if (current.controller.signal.aborted || current.result)
        throw Error("task_stopped");
      if (current.busy) throw Error("operation_in_progress");
      current.busy = true;
      try {
        const task = tasks.get(current.owner, current.id),
          a = body.action;
        if (a.kind === "finish") {
          // Validate before acknowledging, but publish success only after cleanup.
          validateEvidence(task, a);
          current.result = {
            answer: a.answer,
            evidenceIds: a.evidenceIds,
            unresolved: a.unresolved ?? [],
          };
          value = { accepted: true };
        } else {
          tasks.update(task.id, current.generation, {
            actions: task.actions + 1,
          });
          const rawObservation = await current.session.act(a, task);
          const screenshot = rawObservation.screenshot.toString("base64");
          const screenshotSha256 = createHash("sha256")
            .update(rawObservation.screenshot)
            .digest("hex");
          delete rawObservation.screenshot;
          const observation = redactObservation({
            ...rawObservation,
            screenshotSha256,
          });
          tasks.update(task.id, current.generation, {
            evidence: [...task.evidence, observation].slice(-60),
          });
          value = { ...observation, screenshot };
          if (observation.challenge) {
            current.reason = observation.challenge;
            current.controller.abort();
          }
        }
      } catch (e) {
        if (/owner_interference|budget|login_required/.test(e.message)) {
          current.reason = e.message;
          current.controller.abort();
        }
        throw e;
      } finally {
        current.busy = false;
      }
    } else throw Error("unauthorized");
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify(value));
  } catch (e) {
    res.writeHead(400, { "content-type": "application/json" });
    res.end(JSON.stringify({ error: e.message }));
  }
});
if (existsSync(socket)) unlinkSync(socket);
server.listen(socket, () => chmodSync(socket, 0o600));
const timer = setInterval(() => {
  if (!active) {
    const t = tasks.rows.find((t) => t.status === "queued");
    if (t) void execute(structuredClone(t));
  }
}, 250);
let upgrading = false;
const upgradeTimer = setInterval(async () => {
  if (active || upgrading) return;
  tasks.prune();
  upgrading = true;
  try {
    tasks.policy = await checkUpgrade(stateDirectory);
  } catch {
    /* retain last validated policy */
  } finally {
    upgrading = false;
  }
}, 60000);
async function stop() {
  clearInterval(timer);
  clearInterval(upgradeTimer);
  active?.controller.abort();
  await active?.session.close().catch(() => {});
  server.close();
}
process.on("SIGTERM", () => void stop());
process.on("SIGINT", () => void stop());
