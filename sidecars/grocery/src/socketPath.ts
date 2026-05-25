import path from "path";
import fs from "fs";
import os from "os";

export function groceryRuntimeDir(): string {
  const xdg = process.env.XDG_RUNTIME_DIR;
  if (xdg && fs.existsSync(xdg)) return path.join(xdg, "augmentagent");
  return path.join(os.tmpdir(), "augmentagent");
}

export function grocerySocketPath(): string {
  if (process.env.GROCERY_SOCKET) return process.env.GROCERY_SOCKET;
  return path.join(groceryRuntimeDir(), "grocery.sock");
}
