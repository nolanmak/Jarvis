import readline from "node:readline";
import { readFileSync } from "node:fs";
import { rpc } from "./rpc.mjs";
import { createHash } from "node:crypto";
const worker = process.argv.includes("--worker");
const socket = process.env.JARVIS_COMPUTER_SOCKET;
const owner = process.argv[2],
  request = process.argv[3];
const schema = worker
  ? {
      type: "object",
      properties: {
        kind: {
          type: "string",
          enum: [
            "navigate",
            "snapshot",
            "click",
            "type",
            "press",
            "scroll",
            "wait",
            "finish",
          ],
        },
        url: { type: "string" },
        ref: { type: "string" },
        text: { type: "string" },
        key: { type: "string" },
        delta: { type: "number" },
        answer: { type: "string" },
        evidenceIds: { type: "array", items: { type: "string" } },
        unresolved: { type: "array", items: { type: "string" } },
      },
      required: ["kind"],
      additionalProperties: false,
    }
  : {
      type: "object",
      properties: {
        operation: {
          type: "string",
          enum: ["start", "status", "cancel", "resume"],
        },
        goal: { type: "string" },
        hosts: { type: "array", items: { type: "string" } },
        flight: {
          type: "object",
          properties: {
            origin: { type: "string" },
            destination: { type: "string" },
            departureDates: { type: "array", items: { type: "string" } },
          },
          required: ["origin", "destination", "departureDates"],
          additionalProperties: false,
        },
        taskId: { type: "string" },
      },
      required: ["operation"],
      additionalProperties: false,
    };
const name = worker ? "browser_action" : "computer_task";
async function dispatch(req) {
  switch (req.method) {
    case "initialize":
      return {
        protocolVersion: "2024-11-05",
        capabilities: { tools: {} },
        serverInfo: { name: "jarvis-computer", version: "1.0" },
      };
    case "ping":
      return {};
    case "tools/list":
      return {
        tools: [
          {
            name,
            description: worker
              ? "Operate only your task tab. Use observed element refs, never invent them. Every action returns rendered evidence and a screenshot. Finish with your answer, evidence IDs and unresolved limitations."
              : "Delegate an interactive website lookup to Astra in the owner's existing Chrome. Start with a complete goal and exact allowed hostnames; status waits up to 20 seconds. Poll until terminal. No newsletter required. Never treat queued/running as success.",
            inputSchema: schema,
          },
        ],
      };
    case "tools/call": {
      if (req.params?.name !== name) throw Error("unknown_tool");
      const args = req.params.arguments;
      if (
        !args ||
        Object.keys(args).some((key) => !Object.hasOwn(schema.properties, key))
      )
        throw Error("invalid_arguments");
      let token;
      try {
        token = worker
          ? process.env.JARVIS_COMPUTER_TASK_TOKEN
          : readFileSync(process.env.JARVIS_COMPUTER_TOKEN_FILE, "utf8").trim();
      } catch {
        throw Error("computer_worker_unavailable");
      }
      const value = await rpc(
        socket,
        token,
        worker
          ? { action: args }
          : {
              ...args,
              owner,
              request:
                args.operation === "start"
                  ? `${request}:${createHash("sha256")
                      .update(
                        JSON.stringify({
                          goal: args.goal,
                          hosts: args.hosts,
                          flight: args.flight,
                        }),
                      )
                      .digest("hex")}`
                  : request,
            },
      );
      const screenshot = value.screenshot;
      delete value.screenshot;
      const content = [{ type: "text", text: JSON.stringify(value) }];
      if (screenshot)
        content.push({
          type: "image",
          mimeType: "image/jpeg",
          data: screenshot,
        });
      return { content };
    }
    default:
      throw Error("unknown_method");
  }
}
for await (const line of readline.createInterface({ input: process.stdin })) {
  let req;
  try {
    req = JSON.parse(line);
    if (req.id === undefined) continue;
    const result = await dispatch(req);
    process.stdout.write(
      JSON.stringify({ jsonrpc: "2.0", id: req.id, result }) + "\n",
    );
  } catch (e) {
    if (req?.id !== undefined)
      process.stdout.write(
        JSON.stringify({
          jsonrpc: "2.0",
          id: req.id,
          result: {
            isError: true,
            content: [{ type: "text", text: e.message }],
          },
        }) + "\n",
      );
  }
}
