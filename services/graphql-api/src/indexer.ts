/**
 * In-memory indexer for pool analytics. Production deployments should
 * back this with Horizon/Soroban RPC event ingestion.
 *
 * Historical data is retained for 30 days (RETENTION_MS). Older price
 * history and events are pruned on each `indexEvent` call.
 */

const RETENTION_MS = 30 * 24 * 60 * 60 * 1000; // 30 days

export type PoolEventType =
  | "swap"
  | "add_liquidity"
  | "remove_liquidity"
  | "campaign_created"
  | "reward_distributed"
  | "fot_detected";

export interface PoolEvent {
  id: string;
  poolId: string;
  type: PoolEventType;
  timestamp: number;
  payload: Record<string, string | number>;
}

export interface PoolStats {
  poolId: string;
  tokenA: string;
  tokenB: string;
  tvl: number;
  volume24h: number;
  fees24h: number;
  swapCount: number;
  priceDeviationBps: number;
}

export interface Position {
  id: string;
  poolId: string;
  owner: string;
  shares: number;
  valueUsd: number;
}

export interface PricePoint {
  poolId: string;
  timestamp: number;
  price: number;
  feeBps: number;
}

export interface HealthAlert {
  poolId: string;
  metric: string;
  threshold: number;
  currentValue: number;
  firedAt: number;
}

export interface PoolHealth {
  poolId: string;
  healthScore: number;
  tvlScore: number;
  volumeScore: number;
  feeEfficiencyScore: number;
  priceDeviationBps: number;
  status: "healthy" | "warning" | "critical";
  alertsFired: HealthAlert[];
}

export interface AlertConfig {
  poolId: string;
  metric: string;
  thresholdBps: number;
}

export class PoolIndexer {
  private stats = new Map<string, PoolStats>();
  private events: PoolEvent[] = [];
  private positions = new Map<string, Position>();
  private priceHistory: PricePoint[] = [];
  private alertConfigs = new Map<string, AlertConfig>();
  private firedAlerts: HealthAlert[] = [];

  // ── Event ingestion ─────────────────────────────────────────────────────────

  indexEvent(event: PoolEvent): void {
    this.events.push(event);
    this.pruneOldData();

    const stats = this.stats.get(event.poolId) ?? {
      poolId: event.poolId,
      tokenA: String(event.payload["tokenA"] ?? ""),
      tokenB: String(event.payload["tokenB"] ?? ""),
      tvl: 0,
      volume24h: 0,
      fees24h: 0,
      swapCount: 0,
      priceDeviationBps: 0,
    };

    if (event.type === "swap") {
      stats.swapCount += 1;
      stats.volume24h += Number(event.payload["amountIn"] ?? 0);
      stats.fees24h += Number(event.payload["fee"] ?? 0);
      const price = Number(event.payload["price"] ?? 0);
      if (price > 0) {
        this.recordPrice({ poolId: event.poolId, timestamp: event.timestamp, price, feeBps: 30 });
        stats.priceDeviationBps = this.computePriceDeviation(event.poolId, price);
      }
    }
    if (event.type === "add_liquidity") {
      stats.tvl += Number(event.payload["amountA"] ?? 0) + Number(event.payload["amountB"] ?? 0);
    }
    if (event.type === "remove_liquidity") {
      stats.tvl -= Number(event.payload["amountA"] ?? 0) + Number(event.payload["amountB"] ?? 0);
      stats.tvl = Math.max(0, stats.tvl);
    }

    this.stats.set(event.poolId, stats);
    this.checkAlerts(event.poolId, stats);
  }

  // ── Queries ─────────────────────────────────────────────────────────────────

  getPoolStats(poolId?: string): PoolStats[] {
    const all = [...this.stats.values()];
    return poolId ? all.filter((s) => s.poolId === poolId) : all;
  }

  getEvents(poolId?: string, limit = 100): PoolEvent[] {
    const filtered = poolId
      ? this.events.filter((e) => e.poolId === poolId)
      : this.events;
    return filtered.slice(-limit).reverse();
  }

  getPositions(owner?: string): Position[] {
    const all = [...this.positions.values()];
    return owner ? all.filter((p) => p.owner === owner) : all;
  }

  upsertPosition(position: Position): void {
    this.positions.set(position.id, position);
  }

  recordPrice(point: PricePoint): void {
    this.priceHistory.push(point);
  }

  getPriceHistory(poolId: string, from?: number, to?: number): PricePoint[] {
    return this.priceHistory.filter((p) => {
      if (p.poolId !== poolId) return false;
      if (from !== undefined && p.timestamp < from) return false;
      if (to !== undefined && p.timestamp > to) return false;
      return true;
    });
  }

  // ── Health scoring ──────────────────────────────────────────────────────────

  getPoolHealth(poolId: string): PoolHealth | null {
    const stats = this.stats.get(poolId);
    if (!stats) return null;

    // TVL score: 0-100. Pools with TVL > 1M score near 100.
    const tvlScore = Math.min(100, (stats.tvl / 1_000_000) * 100);

    // Volume score: volume/TVL ratio. Healthy is 5–20% daily turnover.
    const volumeRatio = stats.tvl > 0 ? stats.volume24h / stats.tvl : 0;
    const volumeScore = Math.min(100, volumeRatio * 500); // 20% ratio = 100 pts

    // Fee efficiency: fees relative to TVL. Healthy is 0.01–0.1% daily.
    const feeRatio = stats.tvl > 0 ? stats.fees24h / stats.tvl : 0;
    const feeEfficiencyScore = Math.min(100, feeRatio * 100_000); // 0.1% = 100 pts

    // Price deviation penalty: high deviation reduces score.
    const deviationPenalty = Math.min(100, stats.priceDeviationBps / 10);

    const healthScore = Math.max(
      0,
      (tvlScore * 0.4 + volumeScore * 0.35 + feeEfficiencyScore * 0.25) -
        deviationPenalty * 0.5,
    );

    const status: "healthy" | "warning" | "critical" =
      healthScore >= 70
        ? "healthy"
        : healthScore >= 40
          ? "warning"
          : "critical";

    const alertsFired = this.firedAlerts.filter((a) => a.poolId === poolId);

    return {
      poolId,
      healthScore: Math.round(healthScore * 10) / 10,
      tvlScore: Math.round(tvlScore * 10) / 10,
      volumeScore: Math.round(volumeScore * 10) / 10,
      feeEfficiencyScore: Math.round(feeEfficiencyScore * 10) / 10,
      priceDeviationBps: stats.priceDeviationBps,
      status,
      alertsFired,
    };
  }

  // ── Alert configuration ─────────────────────────────────────────────────────

  setAlertConfig(config: AlertConfig): AlertConfig {
    const key = `${config.poolId}:${config.metric}`;
    this.alertConfigs.set(key, config);
    return config;
  }

  removeAlertConfig(poolId: string, metric: string): boolean {
    const key = `${poolId}:${metric}`;
    return this.alertConfigs.delete(key);
  }

  getAlertConfigs(poolId?: string): AlertConfig[] {
    const all = [...this.alertConfigs.values()];
    return poolId ? all.filter((c) => c.poolId === poolId) : all;
  }

  // ── Internals ────────────────────────────────────────────────────────────────

  private computePriceDeviation(poolId: string, currentPrice: number): number {
    const history = this.getPriceHistory(poolId, Date.now() - 3_600_000);
    if (history.length < 2) return 0;
    const twap =
      history.reduce((sum, p) => sum + p.price, 0) / history.length;
    if (twap === 0) return 0;
    return Math.round(Math.abs(currentPrice - twap) / twap * 10_000);
  }

  private checkAlerts(poolId: string, stats: PoolStats): void {
    const configs = this.getAlertConfigs(poolId);
    for (const cfg of configs) {
      let currentValue = 0;
      if (cfg.metric === "price_deviation") {
        currentValue = stats.priceDeviationBps;
      } else if (cfg.metric === "tvl") {
        currentValue = stats.tvl;
      } else if (cfg.metric === "volume24h") {
        currentValue = stats.volume24h;
      }

      if (currentValue > cfg.thresholdBps) {
        const alert: HealthAlert = {
          poolId,
          metric: cfg.metric,
          threshold: cfg.thresholdBps,
          currentValue,
          firedAt: Date.now(),
        };
        this.firedAlerts.push(alert);
        // Retain only the last 100 fired alerts.
        if (this.firedAlerts.length > 100) {
          this.firedAlerts.shift();
        }
      }
    }
  }

  private pruneOldData(): void {
    const cutoff = Date.now() - RETENTION_MS;
    this.events = this.events.filter((e) => e.timestamp > cutoff);
    this.priceHistory = this.priceHistory.filter((p) => p.timestamp > cutoff);
    this.firedAlerts = this.firedAlerts.filter((a) => a.firedAt > cutoff);
  }
}

export const defaultIndexer = new PoolIndexer();

// Seed demo data for local development
defaultIndexer.indexEvent({
  id: "evt-1",
  poolId: "pool-demo",
  type: "swap",
  timestamp: Date.now() - 60_000,
  payload: { amountIn: 1000, fee: 3, tokenA: "XLM", tokenB: "USDC", price: 0.12 },
});
defaultIndexer.indexEvent({
  id: "evt-2",
  poolId: "pool-demo",
  type: "add_liquidity",
  timestamp: Date.now() - 120_000,
  payload: { amountA: 500_000, amountB: 60_000, tokenA: "XLM", tokenB: "USDC" },
});
defaultIndexer.upsertPosition({
  id: "pos-1",
  poolId: "pool-demo",
  owner: "G...DEMO",
  shares: 5000,
  valueUsd: 12_500,
});
defaultIndexer.recordPrice({
  poolId: "pool-demo",
  timestamp: Date.now(),
  price: 0.12,
  feeBps: 30,
});
