import path from "path";
import fs from "fs";

export function fetchRuntimeDir(): string {
  if (process.platform === "darwin") return path.join("/tmp", `augmentagent-${process.getuid?.() ?? 0}`);
  const xdg = process.env.XDG_RUNTIME_DIR;
  if (xdg && fs.existsSync(xdg)) return path.join(xdg, "augmentagent");
  return path.join("/tmp", `augmentagent-${process.getuid?.() ?? 0}`);
}

export function fetchSocketPath(): string {
  if (process.env.FETCH_SOCKET) return process.env.FETCH_SOCKET;
  return path.join(fetchRuntimeDir(), "fetch.sock");
}
