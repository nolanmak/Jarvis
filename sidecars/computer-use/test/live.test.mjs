import { test } from "node:test";
import assert from "node:assert/strict";
import { runFixture } from "../fixture.mjs";
test(
  "actual Astra sees screenshots and completes scoped flight and event fixtures",
  { skip: process.env.LIVE_COMPUTER_TEST !== "1", timeout: 240000 },
  async () => {
    const result = await runFixture(
      process.env.CANDIDATE_MODEL || "gpt-6-astra",
    );
    assert.match(result.answer, /187/);
    assert.match(result.answer, /September 24|2026-09-24/);
    assert.match(result.answer, /September 23|2026-09-23/);
    assert.match(result.answer, /September 25|2026-09-25/);
    assert.match(result.answer, /Founder|19:00|7[ :]*[pP]/);
    assert.ok(result.evidenceIds.length);
  },
);
