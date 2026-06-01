# Derivatives and Perpetual Futures Integration Guide

A guide for building perpetual futures and other derivatives on top of the Soroban AMM.

---

## 1. Overview

The AMM provides two primitives that derivatives protocols need:

1. **TWAP oracle** — a manipulation-resistant index price for mark-to-market and funding rate calculations.
2. **Spot price** — the real-time AMM price used as the mark price for liquidations.

A perpetual futures contract tracks the spot price of an underlying asset. Traders hold long or short positions with leverage. A funding rate mechanism keeps the perpetual price anchored to the index (TWAP) price.

---

## 2. Price Feed Integration

### 2.1 Index Price (TWAP)

The index price is the time-weighted average price over a configurable window. It is resistant to single-block manipulation and is used for:

- Funding rate calculation
- Bankruptcy price determination
- Insurance fund triggers

```rust
#[contractclient(name = "TwapConsumerClient")]
pub trait TwapConsumer {
    fn get_twap_price(env: Env, pool: Address, window_seconds: u64) -> i128;
    fn get_twap_both(env: Env, pool: Address, window_seconds: u64) -> (i128, i128);
}

fn get_index_price(env: &Env, twap_consumer: &Address, pool: &Address) -> i128 {
    // 1-hour TWAP for index price — highly manipulation-resistant
    TwapConsumerClient::new(env, twap_consumer).get_twap_price(pool, &3600_u64)
}
```

### 2.2 Mark Price

The mark price is used for unrealised PnL calculation and liquidation triggers. It is a blend of the AMM spot price and the index price to prevent sudden liquidations from short-term price spikes:

```
mark_price = (spot_price * 3 + index_price) / 4
```

This formula weights the spot price at 75% and the index at 25%, dampening manipulation while staying responsive to real price moves.

```rust
fn get_mark_price(env: &Env, pool: &Address, twap_consumer: &Address) -> i128 {
    let pool_client = AmmPoolClient::new(env, pool);
    let (spot_a, _spot_b) = pool_client.price_ratio();

    let twap_client = TwapConsumerClient::new(env, twap_consumer);
    let index_price = twap_client.get_twap_price(pool, &3600_u64);

    (spot_a * 3 + index_price) / 4
}
```

### 2.3 Snapshot Keeper

The TWAP oracle requires periodic snapshots. For a 1-hour index price, save a snapshot every hour:

```sh
stellar contract invoke \
  --id <TWAP_CONSUMER> \
  --source <KEEPER_KEY> \
  -- save_snapshot \
  --pool <AMM_POOL>
```

---

## 3. Funding Rate Mechanism

The funding rate transfers value between longs and shorts to keep the perpetual price anchored to the index. When the mark price is above the index, longs pay shorts; when below, shorts pay longs.

### 3.1 Funding Rate Formula

```
funding_rate = (mark_price - index_price) / index_price * funding_factor
```

Where `funding_factor` controls the speed of convergence (e.g. `1/8` for an 8-hour funding interval).

In Soroban integer arithmetic (scaled by 1_000_000):

```rust
fn compute_funding_rate(mark_price: i128, index_price: i128) -> i128 {
    // funding_rate in bps (1 bps = 0.01%)
    // funding_factor = 1/8 → divide by 8
    (mark_price - index_price) * 10_000 / index_price / 8
}
```

### 3.2 Funding Payment

Funding is applied every `funding_interval_secs` (e.g. 28,800 seconds = 8 hours):

```rust
fn apply_funding(position_size: i128, funding_rate_bps: i128) -> i128 {
    // Positive rate: long pays short. Negative rate: short pays long.
    position_size * funding_rate_bps / 10_000
}
```

---

## 4. Liquidation Mechanics

### 4.1 Liquidation Trigger

A position is liquidatable when its margin ratio falls below the maintenance margin:

```
margin_ratio = (collateral + unrealised_pnl) / position_notional
liquidatable when margin_ratio < maintenance_margin_bps / 10_000
```

Use the mark price (not spot) for unrealised PnL to prevent liquidation cascades from momentary price spikes.

### 4.2 Liquidation Price

The liquidation price is the mark price at which the position's margin ratio equals the maintenance margin:

```
For a long position:
liquidation_price = entry_price * (1 - (initial_margin - maintenance_margin) / leverage)

For a short position:
liquidation_price = entry_price * (1 + (initial_margin - maintenance_margin) / leverage)
```

### 4.3 Liquidation Process

1. Check `mark_price` against the position's liquidation price.
2. If liquidatable, close the position at the current mark price.
3. Transfer remaining margin (after fees) to the liquidator as a reward.
4. If the position is bankrupt (margin < 0), draw from the insurance fund.

---

## 5. Example Perpetual Futures Contract

```rust
#![no_std]

use soroban_sdk::{contract, contractimpl, contracttype, Address, Env};

const PRICE_SCALE: i128 = 1_000_000;
const BPS_DENOM: i128 = 10_000;
const MAINTENANCE_MARGIN_BPS: i128 = 500;  // 5%
const INITIAL_MARGIN_BPS: i128 = 1_000;    // 10%
const FUNDING_INTERVAL_SECS: u64 = 28_800; // 8 hours
const TWAP_WINDOW_SECS: u64 = 3_600;       // 1-hour index price

#[contracttype]
#[derive(Clone)]
pub struct PerpPosition {
    pub owner: Address,
    pub size: i128,        // positive = long, negative = short (scaled by PRICE_SCALE)
    pub entry_price: i128, // scaled by PRICE_SCALE
    pub collateral: i128,  // in quote token units
    pub last_funding_ts: u64,
}

#[contractclient(name = "AmmPoolClient")]
pub trait AmmPool {
    fn price_ratio(env: Env) -> (i128, i128);
}

#[contractclient(name = "TwapConsumerClient")]
pub trait TwapConsumer {
    fn get_twap_price(env: Env, pool: Address, window_seconds: u64) -> i128;
}

#[contractclient(name = "SepTokenClient")]
pub trait SepToken {
    fn transfer(env: Env, from: Address, to: Address, amount: i128);
    fn transfer_from(env: Env, spender: Address, from: Address, to: Address, amount: i128);
}

#[contract]
pub struct PerpFutures;

#[contractimpl]
impl PerpFutures {
    /// Open a leveraged long or short position.
    ///
    /// - `size` > 0 for long, < 0 for short (in base token units, scaled by PRICE_SCALE).
    /// - `collateral` is the margin deposited in quote token.
    pub fn open_position(
        env: Env,
        trader: Address,
        pool: Address,
        twap_consumer: Address,
        collateral_token: Address,
        size: i128,
        collateral: i128,
    ) -> PerpPosition {
        trader.require_auth();
        assert!(size != 0, "size must be non-zero");
        assert!(collateral > 0, "collateral must be positive");

        let mark_price = Self::compute_mark_price(&env, &pool, &twap_consumer);
        let notional = size.abs() * mark_price / PRICE_SCALE;
        let required_margin = notional * INITIAL_MARGIN_BPS / BPS_DENOM;
        assert!(collateral >= required_margin, "insufficient margin");

        // Pull collateral from trader.
        SepTokenClient::new(&env, &collateral_token).transfer_from(
            &env.current_contract_address(),
            &trader,
            &env.current_contract_address(),
            &collateral,
        );

        PerpPosition {
            owner: trader,
            size,
            entry_price: mark_price,
            collateral,
            last_funding_ts: env.ledger().timestamp(),
        }
    }

    /// Close a position and return remaining collateral to the trader.
    pub fn close_position(
        env: Env,
        position: PerpPosition,
        pool: Address,
        twap_consumer: Address,
        collateral_token: Address,
    ) {
        position.owner.require_auth();

        let mark_price = Self::compute_mark_price(&env, &pool, &twap_consumer);
        let pnl = Self::compute_pnl(&position, mark_price);
        let remaining = position.collateral + pnl;

        if remaining > 0 {
            SepTokenClient::new(&env, &collateral_token).transfer(
                &env.current_contract_address(),
                &position.owner,
                &remaining,
            );
        }
        // If remaining <= 0, position is bankrupt — insurance fund covers the shortfall.
    }

    /// Liquidate an undercollateralised position.
    pub fn liquidate(
        env: Env,
        liquidator: Address,
        position: PerpPosition,
        pool: Address,
        twap_consumer: Address,
        collateral_token: Address,
    ) {
        let mark_price = Self::compute_mark_price(&env, &pool, &twap_consumer);
        let notional = position.size.abs() * mark_price / PRICE_SCALE;
        let pnl = Self::compute_pnl(&position, mark_price);
        let margin = position.collateral + pnl;
        let margin_ratio = margin * BPS_DENOM / notional;

        assert!(margin_ratio < MAINTENANCE_MARGIN_BPS, "position is healthy");

        // Liquidator reward: 50% of remaining margin (capped at 1% of notional).
        let max_reward = notional / 100;
        let reward = (margin / 2).min(max_reward).max(0);

        if reward > 0 {
            SepTokenClient::new(&env, &collateral_token).transfer(
                &env.current_contract_address(),
                &liquidator,
                &reward,
            );
        }
    }

    /// Apply funding payment to a position.
    pub fn settle_funding(
        env: Env,
        position: &mut PerpPosition,
        pool: &Address,
        twap_consumer: &Address,
    ) {
        let now = env.ledger().timestamp();
        let elapsed = now - position.last_funding_ts;
        if elapsed < FUNDING_INTERVAL_SECS {
            return;
        }

        let mark_price = Self::compute_mark_price(&env, pool, twap_consumer);
        let index_price = TwapConsumerClient::new(&env, twap_consumer)
            .get_twap_price(pool, &TWAP_WINDOW_SECS);

        let funding_rate_bps = (mark_price - index_price) * BPS_DENOM / index_price / 8;
        let notional = position.size.abs() * mark_price / PRICE_SCALE;
        let funding_payment = notional * funding_rate_bps / BPS_DENOM;

        // Long pays when mark > index; short pays when mark < index.
        if position.size > 0 {
            position.collateral -= funding_payment;
        } else {
            position.collateral += funding_payment;
        }

        position.last_funding_ts = now;
    }

    fn compute_mark_price(env: &Env, pool: &Address, twap_consumer: &Address) -> i128 {
        let (spot_a, _) = AmmPoolClient::new(env, pool).price_ratio();
        let index = TwapConsumerClient::new(env, twap_consumer)
            .get_twap_price(pool, &TWAP_WINDOW_SECS);
        (spot_a * 3 + index) / 4
    }

    fn compute_pnl(position: &PerpPosition, mark_price: i128) -> i128 {
        let price_delta = mark_price - position.entry_price;
        position.size * price_delta / PRICE_SCALE
    }
}
```

---

## 6. Safety Considerations

### 6.1 Oracle Manipulation

The AMM's circuit breaker pauses the pool when the spot price moves more than 50% in a single ledger. However, derivatives protocols should add additional safeguards:

- Use a blended mark price (spot + TWAP) rather than raw spot.
- Reject liquidations when `is_paused()` returns `true` — the spot price is unreliable.
- Implement a maximum position size relative to pool liquidity to limit the impact of a single trader on the oracle.

### 6.2 Liquidation Cascades

Large liquidations can move the AMM spot price, triggering further liquidations. Mitigate this by:

- Using the mark price (blended) rather than spot for liquidation triggers.
- Implementing a liquidation queue that processes positions in small batches.
- Capping the liquidation size per block.

### 6.3 Funding Rate Extremes

Cap the funding rate to prevent runaway payments during extreme market conditions:

```rust
const MAX_FUNDING_RATE_BPS: i128 = 300; // 3% per 8-hour period

let funding_rate_bps = funding_rate_bps
    .max(-MAX_FUNDING_RATE_BPS)
    .min(MAX_FUNDING_RATE_BPS);
```

### 6.4 Insurance Fund

Maintain an insurance fund to cover bankrupt positions. Fund it with a portion of trading fees (e.g. 10% of the protocol fee). When a position's remaining margin is negative, draw from the insurance fund to cover the shortfall and prevent socialised losses.

### 6.5 Pool Paused State

When the AMM pool is paused (circuit breaker or admin), the spot price is stale. Your derivatives contract should:

- Freeze new position opens.
- Use only the TWAP index price for mark-to-market.
- Delay liquidations until the pool resumes.

```rust
let pool_client = AmmPoolClient::new(&env, &pool);
// Check pause state before any price-sensitive operation
assert!(!pool_client.is_paused(), "pool is paused — operation rejected");
```

---

## 7. Gas Cost Analysis

| Operation | Approximate cost | Notes |
|---|---|---|
| `save_snapshot` (keeper) | ~0.01 XLM | Once per TWAP window |
| `get_twap_price` | ~0.005 XLM | Per price read |
| `price_ratio` (spot) | ~0.002 XLM | Per mark price computation |
| Open position | ~0.02 XLM | Includes collateral transfer |
| Close position | ~0.015 XLM | Includes PnL settlement |
| Liquidate | ~0.015 XLM | Includes reward transfer |
| Settle funding | ~0.01 XLM | Per position per interval |

Keeper cost for 1-hour TWAP snapshots on 5 pools: 24 snapshots/day × 5 pools × 0.01 XLM = **1.2 XLM/day**.

For high-frequency funding settlement (every 8 hours, 1,000 positions): 3 settlements/day × 1,000 × 0.01 XLM = **30 XLM/day**. Batch funding settlement into a single transaction using a keeper contract to reduce per-position overhead.
