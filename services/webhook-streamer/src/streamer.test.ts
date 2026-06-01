/**
 * Unit tests for the webhook-streamer service (issue #306).
 * Run with: node --test dist/streamer.test.js
 */

import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { WebhookRegistry } from "./registry.js";
import type { PoolEvent } from "./types.js";

describe("WebhookRegistry", () => {
  it("registers and lists webhooks", () => {
    const reg = new WebhookRegistry();
    const sub = reg.register("https://example.com/hook");
    assert.equal(reg.size, 1);
    assert.equal(reg.list().length, 1);
    assert.equal(sub.url, "https://example.com/hook");
  });

  it("unregisters a webhook", () => {
    const reg = new WebhookRegistry();
    const sub = reg.register("https://example.com/hook");
    assert.equal(reg.unregister(sub.id), true);
    assert.equal(reg.size, 0);
    assert.equal(reg.unregister(sub.id), false);
  });

  it("filters matching subscriptions by contractId", () => {
    const reg = new WebhookRegistry();
    reg.register("https://a.com", { contractId: "CONTRACT_A" });
    reg.register("https://b.com", { contractId: "CONTRACT_B" });
    reg.register("https://all.com"); // no filter

    const matches = reg.matching("CONTRACT_A", "swap");
    assert.equal(matches.length, 2); // CONTRACT_A + catch-all
    assert.ok(matches.some((s) => s.url === "https://a.com"));
    assert.ok(matches.some((s) => s.url === "https://all.com"));
  });

  it("filters matching subscriptions by eventType", () => {
    const reg = new WebhookRegistry();
    reg.register("https://swap.com", { eventType: "swap" });
    reg.register("https://mint.com", { eventType: "mint_pos" });
    reg.register("https://all.com");

    const matches = reg.matching("ANY_CONTRACT", "swap");
    assert.equal(matches.length, 2); // swap + catch-all
    assert.ok(matches.some((s) => s.url === "https://swap.com"));
    assert.ok(matches.some((s) => s.url === "https://all.com"));
  });

  it("returns empty array when no subscriptions match", () => {
    const reg = new WebhookRegistry();
    reg.register("https://other.com", { contractId: "OTHER" });
    const matches = reg.matching("MY_CONTRACT", "swap");
    assert.equal(matches.length, 0);
  });
});

describe("PoolEvent shape", () => {
  it("conforms to expected interface", () => {
    const event: PoolEvent = {
      id: "evt-1",
      contractId: "CABC123",
      eventType: "swap",
      ledger: 1234,
      timestamp: "2026-06-01T00:00:00Z",
      payload: { amountIn: 1000, amountOut: 990 },
    };
    assert.equal(event.eventType, "swap");
    assert.equal(event.payload["amountIn"], 1000);
  });
});
