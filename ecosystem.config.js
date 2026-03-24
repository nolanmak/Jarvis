module.exports = {
  apps: [
    {
      name: "augmentagent",
      script: "dist/index.js",
      watch: false,
      env: { NODE_ENV: "production" },
      max_memory_restart: "512M",
    },
  ],
};
