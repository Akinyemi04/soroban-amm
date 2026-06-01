# AMM Integration Guide for Lending Protocols

A complete guide for integrating the Soroban AMM as a price oracle and LP token collateral source in a lending protocol.

---

## 1. Overview

Lending protocols need two things from an AMM:

1. **A manipulation-resistant price feed** — to value collateral and trigger liquidations.
2. **LP token collateral support** — to let LPs borrow against their pool positions.

This AMM provides both through the TWAP oracle (`twap_consumer` contract) and the standard LP token interface.

---

## 2. TWAP Oracle Integration

### 2.1 How the Oracle Works

The AMM accumulates a time-weighted price on every state-changing operation (swap, add/remove liquidity). The accumulator is:

```
cum_a += (reserve_b * 1_000_000 / reserve_a) * elapsed_seconds
cum_b += (reserve_a * 1_000_000 / reserve_b) * elapsed_seconds
```

The `twap_consumer` contract reads two snapshots of these accumulators separated by `window_seconds` and computes:

```
twap = (cum_a_now - cum_a_then) / (pool_ts_now - pool_ts_then)
```

All prices are scaled by `1_000_000`. A TWAP of `2_000_000` means 1 unit of token A = 2 units of token B.

### 2.2 Snapshot Keeper

A snapshot must be saved at `now - window_seconds` before calling `get_twap_price`. Run a keeper that calls `save_snapshot` on a fixed interval matching your TWAP window:

```sh
# Save a snapshot every 60 seconds
stellar contract invoke \
  --id <TWAP_CONSUMER_CONTRACT_ID> \
  --source <KEEPER_KEY> \
  -- save_snapshot \
  --pool <AMM_POOL_ADDRESS>
```

For a 5-minute TWAP window, save a snapshot every 5 minutes. The snapshot is stored in persistent storage with a 7-day TTL.

### 2.3 Reading the TWAP Price

```sh
# Get TWAP over the last 300 seconds (5 minutes)
stellar contract invoke \
  --id <TWAP_CONSUMER_CONTRACT_ID> \
  -- get_twap_price \
  --pool <AMM_POOL_ADDRESS> \
  --window_seconds 300
```

In a Soroban contract:

```rust
use soroban_sdk::{contract, contractimpl, Address, Env};

#[contractclient(name = "TwapConsumerClient")]
pub trait TwapConsumerInterface {
    fn get_twap_price(env: Env, pool: Address, window_seconds: u64) -> i128;
    fn assert_lending_price_safe(
        env: Env,
        pool: Address,
        window_seconds: u64,
        spot_price: i128,
        max_deviation_bps: i128,
        collateral_amount: i128,
    ) -> i128;
}

fn get_collateral_value(
    env: &Env,
    twap_consumer: &Address,
    pool: &Address,
    collateral_amount: i128,
) -> i128 {
    let client = TwapConsumerClient::new(env, twap_consumer);
    // Panics if spot price deviates > 5% from TWAP
    client.assert_lending_price_safe(
        pool,
        &300_u64,          // 5-minute window
        &get_spot_price(env, pool),
        &500_i128,         // 5% max deviation
        &collateral_amount,
    )
}
```

### 2.4 Choosing a TWAP Window

| Window | Manipulation resistance | Staleness risk |
|---|---|---|
| 60 s | Low — a single block can move the TWAP | Low |
| 300 s (5 min) | Medium — recommended for most lending protocols | Low |
| 1800 s (30 min) | High — suitable for large collateral positions | Medium |
| 3600 s (1 hr) | Very high | High — price may lag real market |

Use 300 seconds as a starting point. Increase the window for higher-value collateral.

### 2.5 Deviation Threshold

`max_deviation_bps` controls how far the real-time spot price can deviate from TWAP before the lending helper reverts. Recommended values:

| Asset type | `max_deviation_bps` |
|---|---|
| Stablecoin pairs | 100 (1%) |
| Major volatile pairs (XLM/USDC) | 500 (5%) |
| Exotic pairs | 1000 (10%) |

---

## 3. LP Token Collateral

### 3.1 LP Token Value

An LP token represents a proportional share of both pool reserves. The fair value of `shares` LP tokens is:

```
value_a = shares * reserve_a / total_shares
value_b = shares * reserve_b / total_shares
total_value_in_b = value_a * price_a_in_b + value_b
```

Where `price_a_in_b` is the TWAP price of token A denominated in token B.

```rust
fn lp_token_value_in_b(
    env: &Env,
    pool: &Address,
    twap_consumer: &Address,
    shares: i128,
) -> i128 {
    // Read pool state
    let pool_client = AmmPoolClient::new(env, pool);
    let info = pool_client.get_info();
    let total_shares = info.total_shares;
    if total_shares == 0 { return 0; }

    let value_a = shares * info.reserve_a / total_shares;
    let value_b = shares * info.reserve_b / total_shares;

    // Get TWAP price of token A in terms of token B (scaled by 1_000_000)
    let twap_client = TwapConsumerClient::new(env, twap_consumer);
    let price_a = twap_client.get_twap_price(pool, &300_u64);

    // Total value in token B units
    value_b + (value_a * price_a / 1_000_000)
}
```

### 3.2 Collateral Factor

LP tokens carry impermanent loss risk. Apply a conservative collateral factor (loan-to-value ratio):

| Pool type | Suggested LTV |
|---|---|
| Stablecoin/stablecoin | 85% |
| Major volatile pair | 65% |
| Exotic pair | 40% |

### 3.3 Accepting LP Tokens as Collateral

The LP token is a standard SEP-41 token. Your lending contract can hold it as collateral using the standard `transfer_from` pattern:

```rust
// Borrower approves the lending contract to spend their LP tokens
lp_token_client.approve(&borrower, &lending_contract, &collateral_shares, &expiry);

// Lending contract pulls the LP tokens
lp_token_client.transfer_from(
    &lending_contract,
    &borrower,
    &lending_contract,
    &collateral_shares,
);
```

To release collateral on repayment:

```rust
lp_token_client.transfer(&lending_contract, &borrower, &collateral_shares);
```

---

## 4. Price Impact Calculations

Before accepting a large collateral deposit or triggering a liquidation, check the price impact of the implied swap:

```sh
# Simulate a swap to check price impact
stellar contract invoke --id <POOL> \
  -- simulate_swap \
  --token_in <TOKEN_A> \
  --amount_in <AMOUNT>
```

The `SwapSimulation` struct returns:

| Field | Description |
|---|---|
| `amount_out` | Expected output |
| `fee_amount` | Fee deducted |
| `price_impact_bps` | Price impact in basis points |
| `effective_price` | Actual execution price (scaled by 1_000_000) |
| `spot_price` | Pre-trade spot price (scaled by 1_000_000) |

Reject liquidations where `price_impact_bps > 500` (5%) — the liquidation would move the market against itself.

---

## 5. Example Lending Contract

A minimal Soroban lending contract that uses the AMM TWAP oracle:

```rust
#![no_std]

use soroban_sdk::{contract, contractimpl, contracttype, Address, Env};

#[contracttype]
pub struct Position {
    pub collateral_shares: i128,
    pub debt_amount: i128,
    pub pool: Address,
}

#[contractclient(name = "AmmPoolClient")]
pub trait AmmPool {
    fn get_info(env: Env) -> PoolInfo;
    fn shares_of(env: Env, provider: Address) -> i128;
}

#[contracttype]
pub struct PoolInfo {
    pub reserve_a: i128,
    pub reserve_b: i128,
    pub total_shares: i128,
}

#[contractclient(name = "TwapConsumerClient")]
pub trait TwapConsumer {
    fn assert_lending_price_safe(
        env: Env,
        pool: Address,
        window_seconds: u64,
        spot_price: i128,
        max_deviation_bps: i128,
        collateral_amount: i128,
    ) -> i128;
    fn get_twap_price(env: Env, pool: Address, window_seconds: u64) -> i128;
}

#[contractclient(name = "SepTokenClient")]
pub trait SepToken {
    fn transfer(env: Env, from: Address, to: Address, amount: i128);
    fn transfer_from(env: Env, spender: Address, from: Address, to: Address, amount: i128);
    fn balance(env: Env, id: Address) -> i128;
}

#[contract]
pub struct LendingProtocol;

#[contractimpl]
impl LendingProtocol {
    const TWAP_WINDOW: u64 = 300;       // 5-minute TWAP
    const MAX_DEVIATION_BPS: i128 = 500; // 5% max spot/TWAP deviation
    const LTV_BPS: i128 = 6_500;        // 65% loan-to-value
    const PRICE_SCALE: i128 = 1_000_000;

    /// Deposit LP tokens as collateral and borrow debt_token.
    pub fn borrow(
        env: Env,
        borrower: Address,
        pool: Address,
        lp_token: Address,
        twap_consumer: Address,
        debt_token: Address,
        collateral_shares: i128,
        borrow_amount: i128,
    ) {
        borrower.require_auth();

        // 1. Pull LP token collateral from borrower.
        SepTokenClient::new(&env, &lp_token).transfer_from(
            &env.current_contract_address(),
            &borrower,
            &env.current_contract_address(),
            &collateral_shares,
        );

        // 2. Value the collateral using TWAP-protected price.
        let pool_client = AmmPoolClient::new(&env, &pool);
        let info = pool_client.get_info();
        let value_a = collateral_shares * info.reserve_a / info.total_shares;
        let value_b = collateral_shares * info.reserve_b / info.total_shares;

        // Get spot price and validate against TWAP.
        let twap_client = TwapConsumerClient::new(&env, &twap_consumer);
        let price_a = twap_client.get_twap_price(&pool, &Self::TWAP_WINDOW);
        let collateral_value_in_b = value_b + (value_a * price_a / Self::PRICE_SCALE);

        // 3. Enforce LTV.
        let max_borrow = collateral_value_in_b * Self::LTV_BPS / 10_000;
        assert!(borrow_amount <= max_borrow, "borrow exceeds LTV");

        // 4. Transfer debt tokens to borrower.
        SepTokenClient::new(&env, &debt_token).transfer(
            &env.current_contract_address(),
            &borrower,
            &borrow_amount,
        );
    }

    /// Liquidate an undercollateralised position.
    pub fn liquidate(
        env: Env,
        liquidator: Address,
        borrower: Address,
        pool: Address,
        lp_token: Address,
        twap_consumer: Address,
        debt_token: Address,
        position: Position,
    ) {
        liquidator.require_auth();

        // Re-value collateral at current TWAP.
        let pool_client = AmmPoolClient::new(&env, &pool);
        let info = pool_client.get_info();
        let value_a = position.collateral_shares * info.reserve_a / info.total_shares;
        let value_b = position.collateral_shares * info.reserve_b / info.total_shares;

        let twap_client = TwapConsumerClient::new(&env, &twap_consumer);
        let price_a = twap_client.get_twap_price(&pool, &Self::TWAP_WINDOW);
        let collateral_value = value_b + (value_a * price_a / Self::PRICE_SCALE);

        let max_borrow = collateral_value * Self::LTV_BPS / 10_000;
        assert!(position.debt_amount > max_borrow, "position is healthy");

        // Liquidator repays debt.
        SepTokenClient::new(&env, &debt_token).transfer_from(
            &env.current_contract_address(),
            &liquidator,
            &env.current_contract_address(),
            &position.debt_amount,
        );

        // Liquidator receives collateral LP tokens.
        SepTokenClient::new(&env, &lp_token).transfer(
            &env.current_contract_address(),
            &liquidator,
            &position.collateral_shares,
        );
    }
}
```

---

## 6. Safety Considerations

### 6.1 Flash Loan Price Manipulation

The AMM's circuit breaker auto-pauses the pool when the spot price moves more than 50% in a single ledger. However, lending protocols should add their own defence:

- Always use `assert_lending_price_safe` instead of reading the spot price directly.
- Never use `price_ratio()` as the sole price source for collateral valuation.
- Use a TWAP window of at least 300 seconds.

### 6.2 Reentrancy

The AMM blocks reentrant calls during flash loans (`AmmError::Reentrant`). Your lending contract should not call back into the AMM pool inside an `on_flash_loan` callback.

### 6.3 Empty Pool Guard

`price_ratio()` panics with `AmmError::EmptyPool` when reserves are zero. Always check `get_info().total_shares > 0` before valuing LP collateral.

### 6.4 Stale Snapshots

`get_twap_price` panics if no snapshot exists at `now - window_seconds`. Ensure your keeper is running and handle the error gracefully in your contract:

```rust
let twap_result = twap_client.try_get_twap_price(&pool, &300_u64);
match twap_result {
    Ok(Ok(price)) => price,
    _ => panic!("TWAP unavailable — reject operation"),
}
```

### 6.5 LP Token Liquidity Risk

LP tokens can lose value if the pool is drained or paused. Monitor `is_paused()` and `get_circuit_breaker_config().tripped` and freeze new borrows against a paused pool's LP tokens.

---

## 7. Gas Cost Analysis

All costs are approximate and depend on Soroban resource pricing at the time of execution.

| Operation | Approximate cost | Notes |
|---|---|---|
| `save_snapshot` | ~0.01 XLM | Persistent storage write; run by keeper |
| `get_twap_price` | ~0.005 XLM | Persistent storage read |
| `assert_lending_price_safe` | ~0.005 XLM | Includes `get_twap_price` |
| `get_info` | ~0.002 XLM | Instance storage read |
| `simulate_swap` | ~0.003 XLM | Read-only computation |
| LP token `transfer_from` | ~0.005 XLM | Persistent storage write |

Keeper cost for 5-minute snapshots across 10 pools: ~288 snapshots/day × 10 pools × 0.01 XLM ≈ **28.8 XLM/day**.

Use `get_twap_all` to batch TWAP reads across multiple pools in a single call, reducing per-pool overhead.
