import http from "node:http";
export function rpc(socket, token, body) {
  return new Promise((resolve, reject) => {
    const req = http.request(
      {
        socketPath: socket,
        path: "/",
        method: "POST",
        headers: {
          authorization: `Bearer ${token}`,
          "content-type": "application/json",
        },
      },
      (res) => {
        let raw = "";
        res.on("data", (c) => {
          raw += c;
          if (raw.length > 12 * 1024 * 1024)
            req.destroy(Error("response_too_large"));
        });
        res.on("end", () => {
          try {
            const value = JSON.parse(raw);
            if (res.statusCode !== 200) reject(Error(value.error));
            else resolve(value);
          } catch (e) {
            reject(e);
          }
        });
      },
    );
    req.on("error", () => reject(Error("computer_worker_unavailable")));
    req.setTimeout(30000, () => req.destroy());
    req.end(JSON.stringify(body));
  });
}
