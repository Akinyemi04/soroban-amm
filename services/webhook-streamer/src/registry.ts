/**
 * In-memory webhook subscription registry.
 *
 * Production deployments should persist subscriptions to a database.
 */

import type { WebhookSubscription } from "./types.js";

let _nextId = 1;

export class WebhookRegistry {
  private subscriptions = new Map<string, WebhookSubscription>();

  /** Register a new webhook. Returns the created subscription. */
  register(
    url: string,
    opts: { contractId?: string; eventType?: string; secret?: string } = {},
  ): WebhookSubscription {
    const id = String(_nextId++);
    const sub: WebhookSubscription = {
      id,
      url,
      contractId: opts.contractId,
      eventType: opts.eventType,
      secret: opts.secret,
      createdAt: Date.now(),
    };
    this.subscriptions.set(id, sub);
    return sub;
  }

  /** Remove a webhook by ID. Returns true if it existed. */
  unregister(id: string): boolean {
    return this.subscriptions.delete(id);
  }

  /** List all registered webhooks, optionally filtered by contractId. */
  list(contractId?: string): WebhookSubscription[] {
    const all = [...this.subscriptions.values()];
    return contractId ? all.filter((s) => !s.contractId || s.contractId === contractId) : all;
  }

  /** Find subscriptions that match a given event. */
  matching(contractId: string, eventType: string): WebhookSubscription[] {
    return [...this.subscriptions.values()].filter((s) => {
      if (s.contractId && s.contractId !== contractId) return false;
      if (s.eventType && s.eventType !== eventType) return false;
      return true;
    });
  }

  get size(): number {
    return this.subscriptions.size;
  }
}

export const defaultRegistry = new WebhookRegistry();
