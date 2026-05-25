#!/usr/bin/env node
// grocery-weekly.mjs — fired by PM2 on a Sunday cron. POSTs the
// "order groceries for this week" prompt to the dashboard's /api/ask,
// which routes through the agent + grocery tool and ultimately drops an
// approval card into the Discord channel for the user to approve.

import http from "node:http";

const port = process.env.DASHBOARD_PORT || "3000";
const url = `http://localhost:${port}/api/ask`;
const payload = JSON.stringify({ question: "order groceries for this week" });

const req = http.request(
  url,
  {
    method: "POST",
    headers: {
      "content-type": "application/json",
      "content-length": Buffer.byteLength(payload),
    },
  },
  (res) => {
    let body = "";
    res.setEncoding("utf8");
    res.on("data", (c) => (body += c));
    res.on("end", () => {
      console.log(`[grocery-weekly] status=${res.statusCode}`);
      console.log(body.slice(0, 1000));
      process.exit(res.statusCode && res.statusCode < 400 ? 0 : 1);
    });
  },
);

req.on("error", (e) => {
  console.error("[grocery-weekly] error:", e.message);
  process.exit(1);
});

req.write(payload);
req.end();
