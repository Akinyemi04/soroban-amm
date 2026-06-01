// ── Shared types for the webhook-streamer service (issue #306) ──────────────

/** Raw Soroban contract event as returned by Horizon's /events endpoint. */
export interface HorizonEvent {
  id: string;
  type: string;
  ledger: number;
  ledgerClosedAt: string;
  contractId: string;
  topic: string[];
  value: string;
  pagingToken: string;
}

/** Normalised pool event forwarded to webhooks. */
export interface PoolEvent {
  id: string;
  contractId: string;
  eventType: string;
  ledger: number;
  timestamp: string;
  payload: Record<string, unknown>;
}

/** A registered webhook subscription. */
export interface WebhookSubscription {
  id: string;
  url: string;
  /** Filter by contract ID; undefined = all contracts. */
  contractId?: string;
  /** Filter by event type (e.g. "swap", "mint_pos"); undefined = all types. */
  eventType?: string;
  /** Shared secret sent in X-Webhook-Secret header for HMAC verification. */
  secret?: string;
  createdAt: number;
}

/** Result of a single webhook delivery attempt. */
export interface DeliveryResult {
  subscriptionId: string;
  url: string;
  success: boolean;
  statusCode?: number;
  error?: string;
  attemptedAt: number;
}
