export const GATES = [
  "vision",
  "tools",
  "completion",
  "cancellation",
  "policy",
  "flight",
  "general",
];
export function recommendedCandidate(models, guidance) {
  const candidates = models.filter(
    (m) =>
      m.visibility === "list" &&
      /most capable/i.test(m.description ?? "") &&
      m.input_modalities?.includes("image"),
  );
  if (candidates.length !== 1 || !guidance.includes(candidates[0].slug))
    throw Error("unverified_recommendation");
  return candidates[0].slug;
}
export function modelDecision(state) {
  const model = state.mode === "pinned" ? state.pin : state.current;
  if (!/^gpt-[a-zA-Z0-9.-]+$/.test(model ?? "")) throw Error("invalid_model");
  return { model, mode: state.mode };
}
export function promote(state, candidate) {
  const source = new URL(candidate.source);
  if (
    state.mode !== "auto-validated" ||
    source.protocol !== "https:" ||
    !["developers.openai.com", "platform.openai.com"].includes(
      source.hostname,
    ) ||
    !candidate.available ||
    !candidate.revision ||
    !GATES.every((g) => candidate.gates?.[g] === true)
  )
    throw Error("unvalidated_model");
  modelDecision({ mode: "pinned", pin: candidate.model });
  return {
    ...state,
    previous: state.current,
    current: candidate.model,
    validatedAt: new Date().toISOString(),
    validation: candidate,
  };
}
export async function refreshModel(state, deps) {
  if (state.mode !== "auto-validated") return state;
  try {
    const candidate = await deps.discover();
    if (candidate.model === state.current)
      return {
        ...state,
        checkedAt: new Date().toISOString(),
        lastCheck: "current",
      };
    const available = (await deps.available()).includes(candidate.model);
    if (!available) throw Error("candidate_unavailable");
    const gates = await deps.evaluate(candidate.model);
    return {
      ...promote(state, {
        ...candidate,
        available,
        gates,
        revision: deps.revision,
      }),
      lastCheck: "promoted",
    };
  } catch (e) {
    return {
      ...state,
      checkedAt: new Date().toISOString(),
      lastCheck: e.message,
    };
  }
}
