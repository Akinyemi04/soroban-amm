# Soroban AMM Simulator

Off-chain simulation engine for the Soroban AMM pool.

## What it does

- Simulates swaps with the same constant-product math as the on-chain pool
- Replays historical trade logs for backtesting
- Runs Monte Carlo stress tests over a trade set
- Exposes both a Rust library and a CLI

## CLI

```bash
cargo run -p soroban-amm-simulator --bin amm-sim -- quote \
  --pool pool.json \
  --token-in XLM \
  --amount-in 1000000 \
  --pretty

cargo run -p soroban-amm-simulator --bin amm-sim -- replay \
  --pool pool.json \
  --trades trades.json

cargo run -p soroban-amm-simulator --bin amm-sim -- monte-carlo \
  --pool pool.json \
  --trades trades.json \
  --iterations 1000 \
  --amount-shock-bps 50
```

## JSON formats

`pool.json`

```json
{
  "token_a": "XLM",
  "token_b": "USDC",
  "reserve_a": 1000000,
  "reserve_b": 1000000,
  "total_shares": 1000000,
  "fee_bps": 30,
  "protocol_fee_bps": 5
}
```

`trades.json`

```json
[
  {
    "timestamp": 1,
    "kind": "swap_exact_in",
    "token_in": "XLM",
    "amount_in": 100000
  },
  {
    "timestamp": 2,
    "kind": "swap_exact_out",
    "token_out": "USDC",
    "amount_out": 50000,
    "max_in": 60000
  }
]
```
