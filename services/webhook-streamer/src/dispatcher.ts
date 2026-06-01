/**
 * Webhook dispatcher — delivers a PoolEvent to all matching subscribers.
 *
 * Retries up to MAX_RETRIES times with exponential back-off.
 * Sends X-Webhook-Secret header when a secret is configured.
 */

import fetch from "node-fetch";
import type { PoolEvent, WebhookSubscription, DeliveryResult } from "./types.js";
import type { WebhookRegistry } from "./registry.js";

const MAX_RETRIES = 3;
const BASE_DELAY_MS = 500;

export class WebhookDispatcher {
  constructor(private readonly registry: WebhookRegistry) {}

  /** Fan out `event` to all matching subscriptions. Resolves when all
   *  deliveries have been attempted (failures are logged, not thrown). */
  async dispatch(event: PoolEvent): Promise<DeliveryResult[]> {
    const subs = this.registry.matching(event.contractId, event.eventType);
    return Promise.all(subs.map((sub) => this._deliver(sub, event)));
  }

  private async _deliver(
    sub: WebhookSubscription,
    event: PoolEvent,
    attempt = 0,
  ): Promise<DeliveryResult> {
    const headers: Record<string, string> = {
      "Content-Type": "application/json",
    };
    if (sub.secret) {
      headers["X-Webhook-Secret"] = sub.secret;
    }

    try {
      const res = await fetch(sub.url, {
        method: "POST",
        headers,
        body: JSON.stringify(event),
      });

      if (res.ok) {
        return {
          subscriptionId: sub.id,
          url: sub.url,
          success: true,
          statusCode: res.status,
          attemptedAt: Date.now(),
        };
      }

      // Non-2xx: retry if attempts remain.
      if (attempt < MAX_RETRIES) {
        await _sleep(BASE_DELAY_MS * 2 ** attempt);
        return this._deliver(sub, event, attempt + 1);
      }

      return {
        subscriptionId: sub.id,
        url: sub.url,
        success: false,
        statusCode: res.status,
        error: `HTTP ${res.status}`,
        attemptedAt: Date.now(),
      };
    } catch (err) {
      if (attempt < MAX_RETRIES) {
        await _sleep(BASE_DELAY_MS * 2 ** attempt);
        return this._deliver(sub, event, attempt + 1);
      }
      return {
        subscriptionId: sub.id,
        url: sub.url,
        success: false,
        error: String(err),
        attemptedAt: Date.now(),
      };
    }
  }
}

function _sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
