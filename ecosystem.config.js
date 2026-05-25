module.exports = {
  apps: [
    {
      name: "augmentagent",
      script: "dist/index.js",
      watch: false,
      env: { NODE_ENV: "production" },
      max_memory_restart: "512M",
    },
    {
      name: "grocery-sidecar",
      script: "dist/index.js",
      cwd: "sidecars/grocery",
      watch: false,
      env: { NODE_ENV: "production" },
      max_memory_restart: "1G",
      // Restart on crash, but back off if it crashes repeatedly (likely
      // a config error rather than a transient browser hiccup).
      min_uptime: "30s",
      max_restarts: 5,
    },
    {
      name: "fetch-sidecar",
      script: "dist/index.js",
      cwd: "sidecars/fetch",
      watch: false,
      env: { NODE_ENV: "production" },
      max_memory_restart: "1G",
      min_uptime: "30s",
      max_restarts: 5,
    },
    {
      name: "grocery-weekly",
      script: "scripts/grocery-weekly.mjs",
      // PM2 cron: every Sunday at 10:00 local. Posts "order groceries for
      // this week" to the dashboard /api/ask, which routes through the
      // agent and lands an approval card in Discord.
      cron_restart: "0 10 * * 0",
      autorestart: false,
    },
  ],
};
