import dotenv from "dotenv";
dotenv.config();

import express from "express";
import path from "path";
import { initDb } from "./db";
import dashboardRouter from "./dashboard";

const DASHBOARD_PORT = parseInt(process.env.DASHBOARD_PORT || "3000");

function main(): void {
  initDb();

  const app = express();
  app.use(express.json());
  app.use(express.urlencoded({ extended: true }));
  app.use(express.static(path.join(__dirname, "..", "public")));
  app.set("view engine", "ejs");
  app.set("views", path.join(__dirname, "..", "views"));
  app.use(dashboardRouter);

  app.listen(DASHBOARD_PORT, () => {
    console.log(`Dashboard running at http://localhost:${DASHBOARD_PORT}`);
  });
}

main();
