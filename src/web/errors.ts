// Daemon socket error codes and their HTTP statuses (spec §5.2.1).
// This table is the only place the code → HTTP mapping lives.

const HTTP_STATUS: Readonly<Record<string, number>> = {
  bad_request: 400,
  thread_not_found: 404,
  action_not_found: 404,
  turn_not_found: 404,
  thread_archived: 409,
  turn_conflict: 409,
  already_resolved: 409,
  text_too_large: 413,
  thread_busy: 429,
  unsupported_version: 500,
  internal: 500,
  jarvis_offline: 503,
  web_disabled: 503,
  jarvis_timeout: 504,
};

/** HTTP status for a daemon error code; any unknown code is 500. */
export function httpStatusFor(code: string): number {
  return Object.prototype.hasOwnProperty.call(HTTP_STATUS, code) ? HTTP_STATUS[code] : 500;
}

/** Every failure talking to the daemon, keyed by a §5.2.1 code. */
export class DaemonError extends Error {
  readonly code: string;

  constructor(code: string, message?: string) {
    super(message || code);
    this.name = "DaemonError";
    this.code = code;
  }

  get status(): number {
    return httpStatusFor(this.code);
  }
}
