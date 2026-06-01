/**
 * Horizon event poller — subscribes to Soroban contract events via the
 * Horizon /events endpoint and emits normalised PoolEvents.
 *
 * Uses cursor-based pagination so no event is missed across restarts
 * (persist `lastCursor` to a durable store in production).
 */

import fetch from "node-fetch";
import type { HorizonEvent, PoolEvent } from "./types.js";

export type EventHandler = (event: PoolEvent) => Promise<void>;

/** Known event topic prefixes emitted by the AMM contracts. */
const KNOWN_TOPICS: Record<string, string> = {
  swap:      "swap",
  mint_pos:  "mint_pos",
  burn_pos:  "burn_pos",
  coll_fees: "coll_fees",
  mint_1t:   "mint_1t",
  rng_ord:   "rng_ord",
  staked:    "staked",
  unstaked:  "unstaked",
  claimed:   "claimed",
};

export interface PollerOptions {
  /** Horizon base URL, e.g. "https://horizon-testnet.stellar.org" */
  horizonUrl: string;
  /** Contract IDs to subscribe to. */
  contractIds: string[];
  /** Polling interval in milliseconds (default 5000). */
  pollIntervalMs?: number;
  /** Starting cursor ("now" = only new events). */
  startCursor?: string;
}

export class HorizonPoller {
  private running = false;
  private lastCursor: string;
  private readonly opts: Required<PollerOptions>;

  constructor(opts: PollerOptions, private readonly handler: EventHandler) {
    this.opts = {
      pollIntervalMs: 5_000,
      startCursor: "now",
      ...opts,
    };
    this.lastCursor = this.opts.startCursor;
  }

  start(): void {
    if (this.running) return;
    this.running = true;
    void this._loop();
  }

  stop(): void {
    this.running = false;
  }

  private async _loop(): Promise<void> {
    while (this.running) {
      try {
        await this._poll();
      } catch (err) {
        console.error("[HorizonPoller] poll error:", err);
      }
      await _sleep(this.opts.pollIntervalMs);
    }
  }

  private async _poll(): Promise<void> {
    for (const contractId of this.opts.contractIds) {
      const url = this._buildUrl(contractId);
      const res = await fetch(url);
      if (!res.ok) {
        console.warn(`[HorizonPoller] ${contractId}: HTTP ${res.status}`);
        continue;
      }

      const body = (await res.json()) as {
        _embedded?: { records?: HorizonEvent[] };
      };
      const records = body._embedded?.records ?? [];

      for (const raw of records) {
        const event = this._normalise(raw);
        if (event) {
          await this.handler(event);
        }
        this.lastCursor = raw.pagingToken;
      }
    }
  }

  private _buildUrl(contractId: string): string {
    const params = new URLSearchParams({
      contract_id: contractId,
      cursor: this.lastCursor,
      limit: "200",
      order: "asc",
    });
    return `${this.opts.horizonUrl}/events?${params.toString()}`;
  }

  private _normalise(raw: HorizonEvent): PoolEvent | null {
    // The first topic element is the event name (symbol_short).
    const topicName = raw.topic[0] ?? "";
    const eventType = KNOWN_TOPICS[topicName] ?? topicName;

    let payload: Record<string, unknown> = {};
    try {
      payload = JSON.parse(raw.value) as Record<string, unknown>;
    } catch {
      payload = { raw: raw.value };
    }

    return {
      id: raw.id,
      contractId: raw.contractId,
      eventType,
      ledger: raw.ledger,
      timestamp: raw.ledgerClosedAt,
      payload,
    };
  }
}

function _sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
