import dotenv from "dotenv";
dotenv.config();

import express from "express";
import path from "path";
import { initDb } from "./db";
import dashboardRouter from "./dashboard";
import apiV1Router from "./apiV1";
import webhooksRouter from "./webhooks";
import {
  getBindHost,
  getDashboardPort,
  resolveApiKey,
  hostOriginGuard,
  contentSecurityPolicy,
  loginPageHandler,
  loginSubmitHandler,
} from "./security";

const DASHBOARD_PORT = getDashboardPort();
const DASHBOARD_HOST = getBindHost();

function main(): void {
  initDb();

  const app = express();

  // Ensure auth is initialized (generates+persists+logs a key on first run).
  resolveApiKey();

  // #297: DNS-rebinding (Host allow-list) + CSRF (Origin/Referer) guard and
  // strict CSP/security headers, applied before any handler.
  app.use(hostOriginGuard);
  app.use(contentSecurityPolicy);

  // Provider webhooks need the raw body for HMAC verification — mount
  // BEFORE express.json() so the JSON parser doesn't consume the stream.
  app.use(webhooksRouter);

  app.use(express.json());
  app.use(express.urlencoded({ extended: true }));
  app.use(express.static(path.join(__dirname, "..", "public")));
  app.set("view engine", "ejs");
  app.set("views", path.join(__dirname, "..", "views"));

  // #297: unauthenticated login surface (issues the session cookie the UI uses).
  app.get("/login", loginPageHandler);
  app.post("/login", loginSubmitHandler);

  // Routes — versioned JSON API first (split-deployment surface from #1),
  // then the EJS-rendered dashboard UI. Without these mounts, /api/v1/*
  // and /webhooks/* both 404 in production: the dashboard-server entry
  // point had diverged from src/index.ts which mounts them.
  app.use(apiV1Router);
  app.use(dashboardRouter);

  app.listen(DASHBOARD_PORT, DASHBOARD_HOST, () => {
    console.log(`Dashboard running at http://${DASHBOARD_HOST}:${DASHBOARD_PORT}`);
  });
}

main();
