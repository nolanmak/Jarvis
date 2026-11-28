// Turns a thread log (messages + turn rows) into the rows the log fragment
// renders (spec §5.1 "failures are not messages", §5.4). Pure.

import { renderMarkdown } from "./markdown.logic";
import type { ThreadLog, WebMessage, WebTurn } from "./readDb";

export interface LogRow {
  turnId: string;
  owner: WebMessage | null;
  /** Jarvis's answer, already rendered as safe HTML. */
  answerHtml: string | null;
  answerAt: number | null;
  /** The turn when it needs a status row: anything but done. */
  turn: WebTurn | null;
  cancellable: boolean;
  resettable: boolean;
}

/** One row per turn, in message order. A failed or cancelled turn renders from its turn row. */
export function logRows(log: ThreadLog): LogRow[] {
  const turns = new Map(log.turns.map((t) => [t.turn_id, t]));
  const rows = new Map<string, LogRow>();
  for (const m of log.messages) {
    let row = rows.get(m.turn_id);
    if (!row) {
      const turn = turns.get(m.turn_id) ?? null;
      const status = turn?.status;
      row = {
        turnId: m.turn_id,
        owner: null,
        answerHtml: null,
        answerAt: null,
        turn: turn && status !== "done" ? turn : null,
        cancellable: status === "queued" || status === "running",
        resettable: status === "failed" && turn?.error_code === "uncertain_session",
      };
      rows.set(m.turn_id, row);
    }
    if (m.role === "owner") row.owner = m;
    else {
      row.answerHtml = renderMarkdown(m.text);
      row.answerAt = m.created_at;
    }
  }
  return [...rows.values()];
}

/** "9:41 AM" in the server's local time. */
export function clockTime(ms: number): string {
  return new Date(ms).toLocaleTimeString("en-US", { hour: "numeric", minute: "2-digit" });
}
