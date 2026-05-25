import path from "path";
import fs from "fs";
import os from "os";

export function fetchRuntimeDir(): string {
  const xdg = process.env.XDG_RUNTIME_DIR;
  if (xdg && fs.existsSync(xdg)) return path.join(xdg, "augmentagent");
  return path.join(os.tmpdir(), "augmentagent");
}

export function fetchSocketPath(): string {
  if (process.env.FETCH_SOCKET) return process.env.FETCH_SOCKET;
  return path.join(fetchRuntimeDir(), "fetch.sock");
}
