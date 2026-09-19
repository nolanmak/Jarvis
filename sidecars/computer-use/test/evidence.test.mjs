import { test } from "node:test";
import assert from "node:assert/strict";
import { validateEvidence, redactObservation } from "../evidence.mjs";
const base = {
  id: "e",
  url: "https://www.google.com/travel/flights",
  observedAt: new Date().toISOString(),
  text: "San Francisco to Philadelphia departing 2026-09-24, one adult, Economy, one way USD 187 total, baggage unknown",
};
test("flight completion rejects wrong dates, invented prices and stale or invalid observations", () => {
  const task = {
    flight: {
      origin: "San Francisco",
      destination: "Philadelphia",
      departureDates: ["2026-09-24"],
    },
    evidence: [base],
  };
  const result = { answer: "USD 187", evidenceIds: ["e"], unresolved: [] };
  validateEvidence(task, result);
  assert.throws(
    () =>
      validateEvidence(
        {
          ...task,
          evidence: [
            base,
            {
              ...base,
              id: "wrong",
              text: base.text
                .replace("2026-09-24", "2026-09-25")
                .replace("187", "99"),
            },
          ],
        },
        { ...result, answer: "USD 99", evidenceIds: ["e", "wrong"] },
      ),
    /price|date/,
  );
  assert.throws(
    () => validateEvidence(task, { ...result, answer: "USD 99" }),
    /price/,
  );
  assert.throws(
    () =>
      validateEvidence(
        { ...task, flight: { ...task.flight, departureDates: ["2026-09-25"] } },
        result,
      ),
    /date/,
  );
  for (const observedAt of [
    "invalid",
    "2000-01-01T00:00:00Z",
    "2099-01-01T00:00:00Z",
  ])
    assert.throws(
      () =>
        validateEvidence(
          { ...task, evidence: [{ ...base, observedAt }] },
          result,
        ),
      /fresh/,
    );
});
test("evidence redacts unrelated account identifiers and URL credentials", () => {
  const cleaned = redactObservation({
    ...base,
    text: "Account fixture@example.com Bearer not-a-real-token",
    url: "https://example.com/search?q=flights&session=private",
  });
  assert.ok(!JSON.stringify(cleaned).includes("fixture@example.com"));
  assert.ok(!JSON.stringify(cleaned).includes("not-a-real-token"));
  assert.ok(!cleaned.url.includes("private"));
});
