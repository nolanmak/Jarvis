#!/usr/bin/env python3
"""Run the installed-router acceptance fixture against an isolated pinned image.

The synthetic API key and admin login exist only in this short-lived container.
No Runpod credential or paid inference is involved.
"""
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid


IMAGE = "jarvis-9router:0.5.75-runpod-4"


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def api(base, endpoint, body=None, cookie=None):
    headers = {"Content-Type": "application/json"}
    if cookie:
        headers["Cookie"] = cookie
    request = urllib.request.Request(
        base + endpoint,
        data=None if body is None else json.dumps(body).encode(),
        headers=headers,
    )
    with urllib.request.urlopen(request, timeout=10) as response:
        return json.load(response), response.headers.get("Set-Cookie", "").split(";", 1)[0]


def main():
    port = free_port()
    base = f"http://127.0.0.1:{port}"
    name = "jarvis-router-fixture-" + uuid.uuid4().hex[:12]
    password = secrets.token_urlsafe(32)
    command = [
        "docker", "run", "--detach", "--name", name,
        "--add-host", "host.docker.internal:host-gateway",
        "--publish", f"127.0.0.1:{port}:20128",
        "--env", f"INITIAL_PASSWORD={password}",
        "--env", f"JWT_SECRET={secrets.token_hex(32)}",
        "--env", f"API_KEY_SECRET={secrets.token_hex(32)}",
        "--env", f"MACHINE_ID_SALT={secrets.token_hex(32)}",
        "--env", "DATA_DIR=/app/data", "--env", "PORT=20128",
        "--env", "HOSTNAME=0.0.0.0", "--env", "NODE_ENV=production",
        IMAGE,
    ]
    subprocess.run(command, check=True, stdout=subprocess.DEVNULL)
    try:
        for _ in range(90):
            try:
                api(base, "/api/health")
                break
            except (OSError, ValueError, urllib.error.HTTPError):
                time.sleep(1)
        else:
            raise RuntimeError("isolated 9Router did not become healthy")
        _, cookie = api(base, "/api/auth/login", {"password": password})
        if not cookie.startswith("auth_token="):
            raise RuntimeError("isolated 9Router login failed")
        key, _ = api(base, "/api/keys", {"name": "Synthetic CI fixture"}, cookie)
        with tempfile.TemporaryDirectory() as directory:
            config = Path(directory) / "router-fixture.json"
            config.write_text(json.dumps({
                "base_url": base + "/v1",
                "api_key": key["key"],
                "admin_password": password,
                "upstream_host": "host.docker.internal",
            }))
            env = dict(os.environ, JARVIS_TEST_MODEL_ROUTER_CONFIG=str(config))
            subprocess.run([sys.executable, "-m", "unittest",
                            "scripts.tests.model_router_live_test.RouterFailover", "-v"],
                           check=True, env=env)
    finally:
        subprocess.run(["docker", "rm", "--force", name],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)


if __name__ == "__main__":
    main()
