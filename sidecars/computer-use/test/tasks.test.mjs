import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Tasks } from "../tasks.mjs";
import {
  modelDecision,
  promote,
  refreshModel,
  recommendedCandidate,
} from "../models.mjs";
import { checkUrl, checkAction, requestAllowed } from "../policy.mjs";

function fixture(t) {
  const dir = mkdtempSync(join(tmpdir(), "computer-test-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  return dir;
}
const input = {
  goal: "Compare SFO to PHL September 24 2026, one adult, economy, one way",
  hosts: ["www.google.com"],
};
test("trusted requests are idempotent, owner scoped and content bound", (t) => {
  const s = new Tasks(fixture(t));
  const a = s.start("owner", "event", input);
  assert.equal(s.start("owner", "event", input).id, a.id);
  assert.throws(
    () => s.start("owner", "event", { ...input, goal: "changed" }),
    /conflict/,
  );
  assert.throws(() => s.get("other", a.id), /not_found/);
  assert.throws(() => s.start("", "event", input), /unauthorized/);
});
test("restart fences running tasks and resumes once without resetting budget or model", (t) => {
  const dir = fixture(t);
  let s = new Tasks(dir);
  const a = s.start("owner", "e", input);
  const generation = s.claim(a.id);
  s.update(a.id, generation, { actions: 8 });
  s = new Tasks(dir);
  assert.equal(s.get("owner", a.id).status, "needs_action");
  assert.throws(
    () => s.update(a.id, generation, { status: "succeeded" }),
    /stale/,
  );
  const b = s.resume("owner", a.id, "resume1");
  assert.equal(b.actions, 8);
  assert.equal(b.model, "gpt-6-astra");
  assert.equal(s.resume("owner", a.id, "resume1").generation, b.generation);
});
test("cancel fences pending model results and preserves evidence", (t) => {
  const s = new Tasks(fixture(t));
  const a = s.start("owner", "e", input);
  const g = s.claim(a.id);
  s.cancel("owner", a.id);
  assert.throws(() => s.update(a.id, g, { status: "succeeded" }), /stale/);
  assert.equal(s.get("owner", a.id).status, "cancelled");
});
test("completion cannot cite nonexistent evidence or an empty result", (t) => {
  const s = new Tasks(fixture(t));
  const a = s.start("owner", "e", input);
  const g = s.claim(a.id);
  assert.throws(
    () => s.finish(a.id, g, { answer: "$187", evidenceIds: ["fake"] }),
    /evidence/,
  );
});
test("worker model selection ignores parent model and only promotes fully validated candidates", () => {
  const state = {
    mode: "auto-validated",
    current: "gpt-6-astra",
    previous: null,
  };
  assert.equal(modelDecision(state).model, "gpt-6-astra");
  assert.throws(
    () =>
      promote(state, {
        model: "gpt-next",
        available: true,
        source: "https://evil.test",
        gates: {},
      }),
    /unvalidated/,
  );
  assert.throws(
    () =>
      promote(state, {
        model: "gpt-next",
        available: true,
        source: "https://developers.openai.com/api/docs/models",
        gates: { vision: true },
      }),
    /unvalidated/,
  );
  assert.equal(
    modelDecision({ ...state, mode: "pinned", pin: "gpt-6-astra" }).model,
    "gpt-6-astra",
  );
});
test("task URLs and operations cannot expand authority", () => {
  for (const url of [
    "file:///etc/passwd",
    "http://localhost",
    "https://127.0.0.1",
    "https://example.com@evil.test",
    "chrome://settings",
  ])
    assert.throws(() => checkUrl(url, ["example.com"]));
  assert.throws(() => checkUrl("https://other.test", ["example.com"]), /host/);
  assert.throws(
    () =>
      checkAction({ kind: "click", target: "Book and pay" }, { actions: 0 }),
    /consequential/,
  );
  assert.throws(
    () =>
      checkAction(
        { kind: "evaluate", code: "document.cookie" },
        { actions: 0 },
      ),
    /operation/,
  );
  assert.throws(
    () => checkAction({ kind: "snapshot" }, { actions: 60 }),
    /budget/,
  );
});
test("read RPC POST is allowed but purchase POST and state-changing GET are denied", () => {
  assert.equal(
    requestAllowed(
      "https://www.google.com/_/FlightsFrontendUi/data/travel.frontend.flights.FlightsFrontendService/GetBookingResults",
      "POST",
    ),
    true,
  );
  assert.equal(
    requestAllowed(
      "https://www.google.com/_/TravelFrontendUi/data/batchexecute",
      "POST",
    ),
    false,
  );
  assert.equal(
    requestAllowed(
      "https://www.google.com/_/FlightsFrontendUi/data/travel.frontend.flights.FlightsFrontendService/GetShoppingResults",
      "POST",
    ),
    true,
  );
  assert.equal(
    requestAllowed(
      "https://www.google.com/_/FlightsFrontendUi/data/travel.frontend.flights.FlightsFrontendService/BookFlight",
      "POST",
    ),
    false,
  );
  assert.equal(requestAllowed("https://shop.test/purchase", "POST"), false);
  assert.equal(requestAllowed("https://shop.test/delete?id=4", "GET"), false);
});
test("artifact retention expires old finished tasks but preserves active work", (t) => {
  const s = new Tasks(fixture(t));
  const a = s.start("owner", "old", input);
  s.cancel("owner", a.id);
  s.rows[0].createdAt = "2000-01-01T00:00:00Z";
  const b = s.start("owner", "active", input);
  s.prune(Date.now());
  assert.throws(() => s.get("owner", a.id), /not_found/);
  assert.equal(s.get("owner", b.id).status, "queued");
});
test("upgrade discovery uses provider flagship metadata corroborated by official guidance", () => {
  const models = [
    {
      slug: "gpt-99-mini",
      visibility: "list",
      description: "Fast model",
      input_modalities: ["text", "image"],
    },
    {
      slug: "gpt-next",
      visibility: "list",
      description: "Our most capable model",
      input_modalities: ["text", "image"],
    },
  ];
  assert.equal(
    recommendedCandidate(models, 'Use gpt-next; previously model="gpt-old".'),
    "gpt-next",
  );
  assert.throws(
    () => recommendedCandidate(models, "Use gpt-old"),
    /unverified/,
  );
  assert.throws(
    () =>
      recommendedCandidate(
        [...models, { ...models[1], slug: "gpt-ambiguous" }],
        "gpt-next gpt-ambiguous",
      ),
    /unverified/,
  );
});
test("daily discovery retains last good model on outage, rejects failed eval, promotes and preserves pin", async () => {
  const state = {
    mode: "auto-validated",
    current: "gpt-6-astra",
    previous: null,
  };
  const deps = {
    discover: async () => ({
      model: "gpt-next",
      source: "https://developers.openai.com/api/docs/guides/latest-model",
    }),
    available: async () => ["gpt-next"],
    evaluate: async () => ({
      vision: true,
      tools: true,
      completion: true,
      cancellation: true,
      policy: true,
      flight: true,
      general: true,
    }),
    revision: "fixture-revision",
  };
  assert.equal(
    (
      await refreshModel(state, {
        ...deps,
        discover: async () => {
          throw Error("outage");
        },
      })
    ).current,
    "gpt-6-astra",
  );
  assert.equal(
    (
      await refreshModel(state, {
        ...deps,
        evaluate: async () => ({ vision: false }),
      })
    ).current,
    "gpt-6-astra",
  );
  const next = await refreshModel(state, deps);
  assert.equal(next.current, "gpt-next");
  assert.equal(next.previous, "gpt-6-astra");
  assert.equal(
    (await refreshModel({ ...state, mode: "pinned", pin: "gpt-6-astra" }, deps))
      .current,
    "gpt-6-astra",
  );
});

test("airport lookup permits only the reviewed RPC in both query and batch body", () => {
  const url =
    "https://www.google.com/_/FlightsFrontendUi/data/batchexecute?rpcids=H028ib";
  const body = (id) =>
    new URLSearchParams({
      "f.req": JSON.stringify([[[id, '["SFO"]', null, "generic"]]]),
    }).toString();
  assert.equal(requestAllowed(url, "POST", body("H028ib")), true);
  assert.equal(requestAllowed(url, "POST", body("OtherWriteRpc")), false);
  assert.equal(requestAllowed(url, "POST"), false);
});
