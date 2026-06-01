/**
 * Webhook Streamer — entry point (issue #306)
 *
 * Starts an HTTP management API on PORT (default 3001) and a Horizon poller
 * that fans contract events out to registered webhooks.
 *
 * Management API:
 *   POST   /webhooks          – register a webhook
 *   DELETE /webhooks/:id      – unregister a webhook
 *   GET    /webhooks          – list all webhooks
 *   GET    /health            – liveness probe
 *
 * Environment variables:
 *   HORIZON_URL      – Horizon base URL (default: https://horizon-testnet.stellar.org)
 *   CONTRACT_IDS     – comma-separated list of contract IDs to watch
 *   POLL_INTERVAL_MS – polling interval in ms (default: 5000)
 *   PORT             – HTTP port (default: 3001)
 */

import { createServer, IncomingMessage, ServerResponse } from "node:http";
import { defaultRegistry } from "./registry.js";
import { WebhookDispatcher } from "./dispatcher.js";
import { HorizonPoller } from "./horizon-poller.js";
import type { PoolEvent } from "./types.js";

const PORT = Number(process.env["PORT"] ?? 3001);
const HORIZON_URL =
  process.env["HORIZON_URL"] ?? "https://horizon-testnet.stellar.org";
const CONTRACT_IDS = (process.env["CONTRACT_IDS"] ?? "")
  .split(",")
  .map((s) => s.trim())
  .filter(Boolean);
const POLL_INTERVAL_MS = Number(process.env["POLL_INTERVAL_MS"] ?? 5_000);

const dispatcher = new WebhookDispatcher(defaultRegistry);

// ── Horizon poller ──────────────────────────────────────────────────────────

if (CONTRACT_IDS.length > 0) {
  const poller = new HorizonPoller(
    { horizonUrl: HORIZON_URL, contractIds: CONTRACT_IDS, pollIntervalMs: POLL_INTERVAL_MS },
    async (event: PoolEvent) => {
      const results = await dispatcher.dispatch(event);
      const failed = results.filter((r) => !r.success);
      if (failed.length > 0) {
        console.warn(`[dispatcher] ${failed.length} delivery failure(s) for event ${event.id}`);
      }
    },
  );
  poller.start();
  console.log(
    `[poller] watching ${CONTRACT_IDS.length} contract(s) via ${HORIZON_URL}`,
  );
} else {
  console.warn("[poller] no CONTRACT_IDS set — poller not started");
}

// ── HTTP management API ─────────────────────────────────────────────────────

function readBody(req: IncomingMessage): Promise<string> {
  return new Promise((resolve, reject) => {
    let data = "";
    req.on("data", (chunk) => (data += chunk));
    req.on("end", () => resolve(data));
    req.on("error", reject);
  });
}

function json(res: ServerResponse, status: number, body: unknown): void {
  const payload = JSON.stringify(body);
  res.writeHead(status, { "Content-Type": "application/json" });
  res.end(payload);
}

const server = createServer(async (req, res) => {
  const url = req.url ?? "/";
  const method = req.method ?? "GET";

  // GET /health
  if (method === "GET" && url === "/health") {
    return json(res, 200, { status: "ok", webhooks: defaultRegistry.size });
  }

  // GET /webhooks
  if (method === "GET" && url === "/webhooks") {
    return json(res, 200, defaultRegistry.list());
  }

  // POST /webhooks
  if (method === "POST" && url === "/webhooks") {
    try {
      const body = JSON.parse(await readBody(req)) as {
        url?: string;
        contractId?: string;
        eventType?: string;
        secret?: string;
      };
      if (!body.url) {
        return json(res, 400, { error: "url is required" });
      }
      const sub = defaultRegistry.register(body.url, {
        contractId: body.contractId,
        eventType: body.eventType,
        secret: body.secret,
      });
      return json(res, 201, sub);
    } catch {
      return json(res, 400, { error: "invalid JSON" });
    }
  }

  // DELETE /webhooks/:id
  const deleteMatch = url.match(/^\/webhooks\/([^/]+)$/);
  if (method === "DELETE" && deleteMatch) {
    const id = deleteMatch[1]!;
    const removed = defaultRegistry.unregister(id);
    return json(res, removed ? 200 : 404, { removed });
  }

  json(res, 404, { error: "not found" });
});

server.listen(PORT, () => {
  console.log(`[webhook-streamer] management API listening on port ${PORT}`);
});

export { server, dispatcher, defaultRegistry };
