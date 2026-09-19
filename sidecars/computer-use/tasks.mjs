import { mkdirSync, readFileSync, writeFileSync, renameSync } from "node:fs";
import { join } from "node:path";
import { randomUUID, createHash } from "node:crypto";
import { modelDecision } from "./models.mjs";
import { validateEvidence } from "./evidence.mjs";

const hash = (value) =>
  createHash("sha256").update(JSON.stringify(value)).digest("hex");
export class Tasks {
  constructor(
    directory,
    policy = { mode: "auto-validated", current: "gpt-6-astra" },
  ) {
    mkdirSync(directory, { recursive: true, mode: 0o700 });
    this.path = join(directory, "tasks.json");
    this.policy = policy;
    try {
      this.rows = JSON.parse(readFileSync(this.path, "utf8"));
    } catch (e) {
      if (e.code !== "ENOENT") throw e;
      this.rows = [];
    }
    for (const t of this.rows)
      if (["running", "queued"].includes(t.status)) {
        if (t.status === "running" && t.startedAt)
          t.elapsedMs += Math.max(0, Date.now() - Date.parse(t.startedAt));
        t.status = "needs_action";
        t.reason = "worker_restarted";
        t.generation++;
      }
    this.save();
  }
  save() {
    const tmp = this.path + ".tmp";
    writeFileSync(tmp, JSON.stringify(this.rows), { mode: 0o600 });
    renameSync(tmp, this.path);
  }
  prune(now = Date.now()) {
    this.rows = this.rows.filter(
      (t) =>
        ["queued", "running"].includes(t.status) ||
        now - Date.parse(t.createdAt) < 86400000,
    );
    this.save();
  }
  start(owner, request, input) {
    if (!owner || !request) throw Error("unauthorized");
    if (
      typeof input.goal !== "string" ||
      !input.goal.trim() ||
      input.goal.length > 12000 ||
      !Array.isArray(input.hosts) ||
      !input.hosts.length ||
      input.hosts.length > 20 ||
      input.hosts.some((h) => !/^([a-z0-9-]+\.)+[a-z]{2,}$/.test(h))
    )
      throw Error("invalid_task");
    if (
      input.flight &&
      (typeof input.flight.origin !== "string" ||
        typeof input.flight.destination !== "string" ||
        !Array.isArray(input.flight.departureDates) ||
        !input.flight.departureDates.length ||
        input.flight.departureDates.length > 7 ||
        input.flight.departureDates.some(
          (d) =>
            !/^\d{4}-\d{2}-\d{2}$/.test(d) || !Number.isFinite(Date.parse(d)),
        ))
    )
      throw Error("invalid_flight_constraints");
    const existing = this.rows.find(
      (t) => t.owner === owner && t.request === request,
    );
    if (existing) {
      if (existing.inputHash !== hash(input)) throw Error("request_conflict");
      return structuredClone(existing);
    }
    const t = {
      id: randomUUID(),
      owner,
      request,
      inputHash: hash(input),
      ...input,
      model: modelDecision(this.policy).model,
      status: "queued",
      generation: 0,
      actions: 0,
      elapsedMs: 0,
      evidence: [],
      createdAt: new Date().toISOString(),
      resumeEvents: [],
    };
    this.rows.push(t);
    this.save();
    return structuredClone(t);
  }
  get(owner, id) {
    const t = this.rows.find((t) => t.id === id && t.owner === owner);
    if (!t) throw Error("not_found");
    return structuredClone(t);
  }
  claim(id) {
    const t = this.rows.find((t) => t.id === id);
    if (t?.status !== "queued") throw Error("not_queued");
    t.status = "running";
    t.startedAt = new Date().toISOString();
    t.generation++;
    this.save();
    return t.generation;
  }
  update(id, generation, patch) {
    const t = this.rows.find((t) => t.id === id);
    if (!t || t.generation !== generation || t.status !== "running")
      throw Error("stale");
    Object.assign(t, patch);
    this.save();
    return structuredClone(t);
  }
  cancel(owner, id) {
    const t = this.rows.find((t) => t.id === this.get(owner, id).id);
    t.generation++;
    t.status = "cancelled";
    t.reason = "owner_cancelled";
    this.save();
    return structuredClone(t);
  }
  resume(owner, id, event) {
    const t = this.rows.find((t) => t.id === this.get(owner, id).id);
    if (t.resumeEvents.includes(event)) return structuredClone(t);
    if (t.status !== "needs_action") throw Error("not_resumable");
    if (t.actions >= 60 || t.elapsedMs >= 300000)
      throw Error("budget_exhausted");
    t.resumeEvents.push(event);
    t.status = "queued";
    delete t.reason;
    this.save();
    return structuredClone(t);
  }
  finish(id, generation, result) {
    const t = this.rows.find((t) => t.id === id);
    validateEvidence(t, result);
    return this.update(id, generation, {
      status: result.unresolved?.length ? "partial" : "succeeded",
      result,
    });
  }
}
