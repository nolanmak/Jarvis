const prices = (text) =>
  [...text.matchAll(/(?:USD\s*|\$)(\d[\d,]*(?:\.\d{1,2})?)/g)].map((m) =>
    Number(m[1].replaceAll(",", "")),
  );
export function validateEvidence(task, result, now = Date.now()) {
  if (
    typeof result.answer !== "string" ||
    !result.answer.trim() ||
    !Array.isArray(result.evidenceIds) ||
    !result.evidenceIds.length
  )
    throw Error("completion_requires_observed_evidence");
  const evidence = result.evidenceIds.map((id) =>
    task.evidence.find((e) => e.id === id),
  );
  if (
    evidence.some(
      (e) =>
        !e ||
        e.challenge ||
        e.text.trim().length <= 40 ||
        !Number.isFinite(Date.parse(e.observedAt)) ||
        now - Date.parse(e.observedAt) > 15 * 60000 ||
        Date.parse(e.observedAt) > now + 5000,
    )
  )
    throw Error("completion_requires_fresh_evidence");
  const text = evidence.map((e) => e.text).join("\n");
  const observed = prices(text);
  if (prices(result.answer).some((p) => !observed.includes(p)))
    throw Error("price_not_in_evidence");
  if (task.flight && !result.unresolved?.length) {
    const f = task.flight;
    const sources = evidence.map((e) => {
      let query = "";
      try {
        query = Buffer.from(
          new URL(e.url).searchParams.get("tfs") ?? "",
          "base64url",
        ).toString();
      } catch {}
      return { text: e.text + " " + query, prices: prices(e.text) };
    });
    const source = sources.map((e) => e.text).join("\n");
    if (
      !source.toLowerCase().includes(f.origin.toLowerCase()) ||
      !source.toLowerCase().includes(f.destination.toLowerCase())
    )
      throw Error("flight_route_not_verified");
    const matchingPrices = [];
    for (const date of f.departureDates) {
      const d = new Date(date + "T12:00:00Z");
      const readable = d.toLocaleDateString("en-US", {
        month: "long",
        day: "numeric",
        year: "numeric",
        timeZone: "UTC",
      });
      const matching = sources.filter(
        (e) =>
          (e.text.includes(date) || e.text.includes(readable)) &&
          e.text.toLowerCase().includes(f.origin.toLowerCase()) &&
          e.text.toLowerCase().includes(f.destination.toLowerCase()),
      );
      if (!matching.some((e) => e.prices.length))
        throw Error("flight_date_not_verified");
      matchingPrices.push(...matching.flatMap((e) => e.prices));
    }
    if (!observed.length) throw Error("flight_fares_not_observed");
    if (prices(result.answer).some((p) => !matchingPrices.includes(p)))
      throw Error("price_not_in_matching_flight_evidence");
  }
}
export function redactObservation(observation) {
  const redact = (s) =>
    s
      .replace(/[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}/gi, "[email redacted]")
      .replace(/\b(Bearer\s+)[\w.-]+/gi, "$1[redacted]");
  const clean = JSON.parse(redact(JSON.stringify(observation)));
  if (clean.url) {
    const u = new URL(clean.url);
    for (const key of [...u.searchParams.keys()])
      if (/token|password|secret|auth|session|code/i.test(key))
        u.searchParams.delete(key);
    clean.url = u.toString();
  }
  return clean;
}
