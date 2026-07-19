import dotenv from "dotenv";
dotenv.config();

// #375 — prefix every console line with a UTC timestamp. The dashboard's
// stdout/stderr land in append-only files via systemd; without timestamps,
// failures (e.g. the stale-key OAuth 401s #375 chased) can't be dated
// against key rotations or restarts. Installed before any other import
// runs so early startup logging is covered too.
for (const level of ["log", "info", "warn", "error"] as const) {
  const orig = console[level].bind(console);
  console[level] = (...args: unknown[]) =>
    orig(`[${new Date().toISOString()}]`, ...args);
}

import express from "express";
import path from "path";
import { initDb } from "./db";
import dashboardRouter from "./dashboard";
import apiV1Router from "./apiV1";
import webhooksRouter from "./webhooks";

const DASHBOARD_PORT = parseInt(process.env.DASHBOARD_PORT || "3000");

function main(): void {
  // #360: honor AUGMENTAGENT_DB so a tenant/test DB isn't silently ignored in
  // favor of the default ./data.db. `initDb` falls back to the default when
  // this is undefined.
  initDb(process.env.AUGMENTAGENT_DB);

  const app = express();

  // Provider webhooks need the raw body for HMAC verification — mount
  // BEFORE express.json() so the JSON parser doesn't consume the stream.
  app.use(webhooksRouter);

  app.use(express.json());
  app.use(express.urlencoded({ extended: true }));
  app.use(express.static(path.join(__dirname, "..", "public")));
  app.set("view engine", "ejs");
  app.set("views", path.join(__dirname, "..", "views"));

  // Routes — versioned JSON API first (split-deployment surface from #1),
  // then the EJS-rendered dashboard UI. Without these mounts, /api/v1/*
  // and /webhooks/* both 404 in production: the dashboard-server entry
  // point had diverged from src/index.ts which mounts them.
  app.use(apiV1Router);
  app.use(dashboardRouter);

  app.listen(DASHBOARD_PORT, () => {
    console.log(`Dashboard running at http://localhost:${DASHBOARD_PORT}`);
  });
}

main();
