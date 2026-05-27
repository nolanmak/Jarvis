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
      name: "fetch-sidecar",
      script: "dist/index.js",
      cwd: "sidecars/fetch",
      watch: false,
      env: { NODE_ENV: "production" },
      max_memory_restart: "1G",
      min_uptime: "30s",
      max_restarts: 5,
    },
  ],
};
