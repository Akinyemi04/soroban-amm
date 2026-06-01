# Soroban AMM GraphQL Analytics API

Indexes pool events and exposes analytics queries with sub-100ms in-memory reads.

## Quick start

```bash
cd services/graphql-api
npm install
npm run dev
```

Open http://localhost:4000 for the Apollo Sandbox.

## Example queries

### Pool TVL, volume, and fees

```graphql
query PoolOverview {
  poolStats(poolId: "pool-demo") {
    poolId
    tokenA
    tokenB
    tvl
    volume24h
    fees24h
    swapCount
  }
}
```

### Position tracking

```graphql
query MyPositions {
  positions(owner: "G...DEMO") {
    id
    poolId
    shares
    valueUsd
  }
}
```

### Historical price and fee data

```graphql
query PriceHistory {
  priceHistory(poolId: "pool-demo", from: 1700000000) {
    timestamp
    price
    feeBps
  }
}
```

### Complex nested query

```graphql
query PoolDashboard($poolId: ID!) {
  poolStats(poolId: $poolId) {
    tvl
    volume24h
    fees24h
  }
  poolEvents(poolId: $poolId, limit: 50) {
    type
    timestamp
    payload
  }
  priceHistory(poolId: $poolId) {
    price
    timestamp
  }
}
```

### Event subscription

```graphql
subscription OnSwap($poolId: ID) {
  poolEvent(poolId: $poolId) {
    id
    type
    timestamp
    payload
  }
}
```

## Production indexing

Wire `PoolIndexer.indexEvent` to your Horizon/Soroban RPC ingestion pipeline for:
- `swap`, `add_liquidity`, `remove_liquidity` (AMM contract)
- `campaign_created`, `reward_distributed` (incentive campaigns)
