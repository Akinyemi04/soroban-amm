# Soroban AMM

[![CI](https://github.com/promisszn/soroban-amm/workflows/CI/badge.svg)](https://github.com/promisszn/soroban-amm/actions)

A full-stack AMM protocol built on Stellar's Soroban smart contract platform. It ships a battle-tested V2 constant-product pool today and is actively building a V3-style concentrated liquidity engine — the only open-source implementation of its kind on Stellar.

---

## Table of Contents

- [Why This Project](#why-this-project)
- [Overview](#overview)
- [Architecture](#architecture)
- [Contracts](#contracts)
  - [AMM Pool Contract](#amm-pool-contract)
  - [LP Token Contract](#lp-token-contract)
  - [Factory Contract](#factory-contract)
  - [Governance Contract](#governance-contract)
  - [TWAP Consumer Contract](#twap-consumer-contract)
  - [Concentrated Liquidity Contract](#concentrated-liquidity-contract)
- [Math & Formulas](#math--formulas)
- [Getting Started](#getting-started)
  - [Prerequisites](#prerequisites)
  - [Build](#build)
  - [Test](#test)
- [Usage](#usage)
  - [Deploy via Factory](#deploy-via-factory)
  - [Deploy Manually](#deploy-manually)
  - [Add Liquidity](#add-liquidity)
  - [Swap Tokens](#swap-tokens)
  - [Remove Liquidity](#remove-liquidity)
  - [Query the Pool](#query-the-pool)
  - [Use the TWAP Oracle](#use-the-twap-oracle)
  - [TypeScript Client Example](#typescript-client-example)
  - [Python Client Example](#python-client-example)
- [Contributing](#contributing)
- [Changelog](#changelog)
- [Security](#security)
- [License](#license)

---

## Why This Project

Stellar has fast finality (~5 seconds), sub-cent fees, and a large existing user base around stablecoins and remittances. Its native DEX is order-book based — a constant-product AMM is a fundamentally different and more composable liquidity model that fills a real gap in the ecosystem.

Several AMM protocols already exist on Stellar, but each has meaningful limitations:

| Protocol | Pool model | Governance | Open source |
|---|---|---|---|
| **Soroswap** | V2 constant-product only | None | Yes |
| **Phoenix** | V2 + stable pools | None | Yes |
| **Sushi** | Concentrated liquidity | None | No (Sushi mainline) |
| **Aquarius** | Governance layer only | AQUA token | Partial |
| **This project** | V2 now + V3 CL in progress | On-chain, LP-token-governed | Yes |

What makes this project different:

- **Concentrated liquidity (V3) in development** — the only open-source Soroban implementation targeting tick-based range positions, the same capital-efficiency model pioneered by Uniswap v3. LPs can earn more fees by concentrating capital in active price ranges instead of spreading it across an infinite curve.
- **On-chain governance** — LP token holders can propose and vote on fee changes directly through a governance contract, with configurable quorum, voting windows, minimum stake, vote locking, and proposal cancellation. No protocol is governed off-chain or by a single admin key.
- **TWAP oracle** — a manipulation-resistant time-weighted average price feed that other protocols (lending markets, derivatives) can build on top of, without needing a separate oracle network.
- **Flash loans** — single-transaction borrowing from pool reserves with a configurable fee, enabling arbitrage, collateral swaps, and liquidation bots.
- **Full composability** — every contract in the protocol is independently deployable and interoperable. The factory, governance, and TWAP contracts can be used with any pool, not just the ones deployed here.

---

## Overview

The protocol lets users:

- **Provide liquidity** — deposit two tokens into a pool and receive LP tokens representing their share of the reserves (V2), or mint a tick-range position for concentrated capital efficiency (V3).
- **Swap tokens** — exchange one pool token for the other at a price determined by the pool's invariant, with slippage protection.
- **Redeem liquidity** — burn LP tokens (V2) or close a position (V3) to withdraw a proportional share of reserves plus accrued fees.
- **Govern the protocol** — stake LP tokens to propose fee changes; vote on active proposals; execute passing proposals on-chain.
- **Access price data** — query a TWAP oracle for a manipulation-resistant average price over any window.

---

## Architecture

The project is a Cargo workspace with five contracts:

```
soroban-amm/
├── Cargo.toml                        # Workspace root
└── contracts/
    ├── amm/                          # V2 constant-product AMM pool
    │   └── src/lib.rs
    ├── token/                        # SEP-41 LP token contract
    │   └── src/lib.rs
    ├── factory/                      # Pool factory and registry
    │   └── src/lib.rs
    ├── governance/                   # On-chain LP governance
    │   └── src/lib.rs
    ├── twap_consumer/                # TWAP oracle consumer
    │   └── src/lib.rs
    └── concentrated_liquidity/       # V3-style tick-based AMM (in progress)
        └── src/lib.rs
```

The V2 AMM contract depends on the token contract — adding or removing liquidity mints or burns LP shares via the token contract. The factory deploys and initialises an AMM + LP token pair in a single transaction. The governance contract holds a reference to a pool and allows LP token holders to vote on parameter changes. The TWAP consumer reads cumulative price state from any AMM pool. The concentrated liquidity contract is standalone — it does not use the LP token and manages positions internally.

---

## Contracts

---

## Storage Layout

### AMM Pool Contract

| Key | Storage Tier | Type | Description |
|---|---|---|---|
| `TokenA` | Instance | `Address` | First pool asset |
| `TokenB` | Instance | `Address` | Second pool asset |
| `LpToken` | Instance | `Address` | LP token contract |
| `ReserveA` | Instance | `i128` | Current TokenA reserves |
| `ReserveB` | Instance | `i128` | Current TokenB reserves |
| `TotalShares` | Instance | `i128` | Total LP shares issued |
| `FeeBps` | Instance | `i128` | Swap fee in basis points |

### LP Token Contract

| Key | Storage Tier | Type | Description |
|---|---|---|---|
| `Admin` | Instance | `Address` | Contract administrator (the AMM pool) |
| `Name` | Instance | `String` | Token name |
| `Symbol` | Instance | `String` | Token symbol |
| `Decimals` | Instance | `u32` | Token decimal places |
| `TotalSupply` | Instance | `i128` | Total shares in circulation |
| `Balance(Address)` | Persistent | `i128` | Individual user share balance |
| `Allowance(Address, Address)` | Persistent | `i128` | Third-party spending allowance |

### Concentrated Liquidity Contract

| Key | Storage Tier | Type | Description |
|---|---|---|---|
| `TokenA` | Instance | `Address` | First pool asset |
| `TokenB` | Instance | `Address` | Second pool asset |
| `FeeBps` | Instance | `i128` | Pool fee in basis points |
| `CurrentTick` | Instance | `i32` | Active tick index |
| `FeeGrowthGlobalA` | Instance | `i128` | Cumulative fee per liquidity unit for token A |
| `FeeGrowthGlobalB` | Instance | `i128` | Cumulative fee per liquidity unit for token B |
| `ActiveLiquidity` | Instance | `i128` | Total liquidity active at the current tick |
| `Position(Address, i32, i32)` | Instance | `Position` | Per-user position keyed by `(owner, lower_tick, upper_tick)` |

---

## Upgrade Considerations

- **Storage Immutability**: Critical setup parameters (e.g., `TokenA`, `TokenB`, `LpToken`) are immutable after `initialize`.
- **Breaking Changes**: Modifying `DataKey` variants or data types constitutes a breaking change. Since Soroban storage is keyed by the enum's binary representation, any restructuring requires a new deployment or a careful migration strategy.
- **State Migration**: Upgrading logic while preserving state is possible via contract code upgrades, but changing storage tiers (e.g., Instance to Persistent) requires manual data relocation.

---

## Public Interface

| Key | Type | Description |
|---|---|---|
| `TokenA` | `Address` | First pool asset |
| `TokenB` | `Address` | Second pool asset |
| `LpToken` | `Address` | LP token contract |
| `ReserveA` | `i128` | Pool's current balance of TokenA |
| `ReserveB` | `i128` | Pool's current balance of TokenB |
| `TotalShares` | `i128` | Total LP shares outstanding |
| `Shares(Address)` | `i128` | LP shares held by a specific provider |
| `FeeBps` | `i128` | Swap fee in basis points (e.g. `30` = 0.30%) |
| `Paused` | `bool` | Emergency circuit breaker state |
| `FlashLoanFeeBps` | `i128` | Flash-loan fee in basis points; defaults to `FeeBps` |

### AMM Pool Contract

Located in [contracts/amm/src/lib.rs](contracts/amm/src/lib.rs).

| Function | Description |
|---|---|
| `initialize(token_a, token_b, lp_token, fee_bps)` | One-time pool setup |
| `initialize_with_flash_loan_fee(token_a, token_b, lp_token, fee_bps, flash_loan_fee_bps)` | One-time pool setup with a distinct flash-loan fee |
| `pause(admin)` | Pause state-changing pool operations; requires `admin` auth |
| `unpause(admin)` | Resume state-changing pool operations; requires `admin` auth |
| `is_paused() → bool` | Read the current pause state |
| `flash_loan(receiver, token, amount, data) → fee` | Borrow pool reserves and repay within the receiver callback |
| `add_liquidity(provider, amount_a, amount_b, min_shares, deadline) → shares` | Deposit tokens, receive LP shares |
| `remove_liquidity(provider, shares, min_a, min_b, deadline) → (a, b)` | Burn LP shares, withdraw tokens |
| `swap(trader, token_in, amount_in, min_out, deadline) → amount_out` | Exchange tokens |
| `swap_exact_out(trader, token_out, amount_out, max_in, deadline) → amount_in` | Buy an exact amount of output token |
| `get_amount_out(token_in, amount_in) → amount_out` | Quote a swap without executing it |
| `get_amount_in(token_out, amount_out) → amount_in` | Quote the input required for an exact output |
| `simulate_swap(token_in, amount_in) → SwapSimulation` | Detailed quote including fee breakdown and price impact |
| `get_info() → PoolInfo` | Read pool state — reserves, fees, total shares, admin, fee recipient, protocol fee |
| `get_accrued_fees() → (i128, i128)` | Read pending protocol fees in `(token_a, token_b)` without moving funds |
| `get_protocol_fee() → (Option<Address>, i128)` | Read the protocol fee recipient and rate |
| `set_protocol_fee(admin, recipient, protocol_fee_bps)` | Configure protocol fee collection |
| `withdraw_protocol_fees() → (i128, i128)` | Transfer accrued protocol fees to the recipient |
| `shares_of(provider) → shares` | Read an LP's share balance |
| `price_ratio() → (i128, i128)` | Read the spot price ratio for both directions |
| `get_price_cumulative() → (i128, i128, u64)` | Read cumulative price accumulators for TWAP computation |
| `update_fee(new_fee_bps)` | Update the swap fee (no-admin variant; emits event) |
| `propose_admin(current_admin, new_admin)` | Nominate a new admin; emits `admin_nominated` |
| `accept_admin(new_admin)` | Nominee accepts the role; emits `admin_changed` |
| `upgrade(new_wasm_hash)` | Upgrade the contract code; requires admin auth |

### Factory Contract

Located in [contracts/factory/src/lib.rs](contracts/factory/src/lib.rs).

A single-entry-point contract for creating and discovering AMM pools. The factory deploys a new AMM pool and its paired LP token in one transaction, enforces uniqueness per token pair, and maintains a registry of all pools it has deployed.

#### Storage

| Key | Type | Description |
|---|---|---|
| `Admin` | `Address` | Factory administrator; set as AMM fee recipient |
| `AmmWasmHash` | `BytesN<32>` | WASM hash of the AMM pool contract |
| `TokenWasmHash` | `BytesN<32>` | WASM hash of the LP token contract |
| `Pool(Address, Address)` | `Address` | Normalised token pair → pool address |
| `AllPools` | `Vec<Address>` | Ordered list of all deployed pool addresses |
| `PoolCount` | `u64` | Monotonic counter used to derive deploy salts |

#### Public Interface

| Function | Description |
|---|---|
| `initialize(admin, amm_wasm_hash, token_wasm_hash)` | One-time factory setup |
| `create_pool(token_a, token_b, fee_bps) → Address` | Deploy a new AMM + LP token pair; panics on duplicate |
| `get_pool(token_a, token_b) → Option<Address>` | Look up an existing pool (order-independent) |
| `get_lp_token(pool) → Option<Address>` | Look up the LP token for a given pool address |
| `all_pools() → Vec<Address>` | List every pool deployed by this factory |
| `get_pool_count() → u64` | Return the total number of deployed pools |
| `get_pools(offset, limit) → Vec<Address>` | Return a paginated page of pool addresses starting at offset |
| `update_wasm_hashes(amm_wasm_hash, token_wasm_hash)` | Update the WASM hashes used for future pool deployments |
| `upgrade(new_wasm_hash)` | Upgrade the factory contract code; requires admin auth |

#### Notes

- Token pair order is **normalised** at creation time (smaller address stored first). `get_pool` accepts either order.
- `create_pool` panics with `"pool already exists"` if a pool for the pair is already registered.
- The factory admin is set as the AMM's `fee_recipient`; protocol fees start at 0 bps and can be enabled later.

---

### Governance Contract

Located in [contracts/governance/src/lib.rs](contracts/governance/src/lib.rs).

Allows LP token holders to propose and vote on parameter changes to a pool on-chain. Proposals are time-locked and require a quorum of voting power to pass.

#### Public Interface

| Function | Description |
|---|---|
| `initialize(amm, lp_token, voting_period, quorum_bps, min_proposer_stake_bps)` | One-time governance setup |
| `set_min_proposer_stake_bps(new_bps)` | Update the minimum LP stake required to create a proposal |
| `propose(proposer, kind) → u32` | Create a new proposal with the specified ProposalKind; returns proposal ID |
| `vote(voter, proposal_id, support)` | Cast a for/against vote weighted by the voter's LP balance |
| `execute(proposal_id)` | Execute a passing proposal after the voting period ends |
| `cancel_proposal(proposal_id, proposer)` | Cancel a pending proposal before voting ends |
| `unlock_vote(voter, proposal_id)` | Release vote-locked LP tokens after a proposal is resolved |
| `get_proposal(proposal_id) → Proposal` | Read proposal details |
| `proposal_status(proposal_id) → ProposalStatus` | Read the current status of a proposal |
| `get_vote_info(proposal_id, voter) → VoteRecord` | Read a specific voter's record on a proposal |
| `get_params() → GovernanceParams` | Read current governance configuration |

#### Notes

- Voting power is snapshotted at the time `vote` is called, based on current LP token balance.
- LP tokens used to vote are locked until `unlock_vote` is called after the proposal resolves.
- A proposal passes if `for_votes / total_supply ≥ quorum_bps / 10_000` and `for_votes > against_votes`.
- Only the original proposer can cancel a proposal, and only before the voting period ends.

---

### TWAP Consumer Contract

Located in [contracts/twap_consumer/src/lib.rs](contracts/twap_consumer/src/lib.rs).

An integration contract that reads the AMM's cumulative price oracle and computes a fixed-window TWAP. Lending protocols, derivatives, and any contract needing an on-chain price feed can use this as a reference or deploy it directly.

| Function | Description |
|---|---|
| `save_snapshot(pool)` | Stores `(cum_a, cum_b, pool_ts)` under `Snapshot(pool, pool_ts)` |
| `get_twap_price(pool, window_seconds) → i128` | Returns `(cum_a_now - cum_a_then) / window_seconds`, where `cum_a_then` comes from the snapshot at `now_ts - window_seconds` |
| `validate_price(spot_price, twap_price, max_deviation_bps) → PriceValidation` | Compares a real-time price against TWAP and flags deviations above a configurable basis-point threshold |
| `validate_price_against_twap(pool, window_seconds, spot_price, max_deviation_bps) → PriceValidation` | Reads TWAP from saved snapshots and validates the supplied AMM spot price |
| `assert_lending_price_safe(pool, window_seconds, spot_price, max_deviation_bps, collateral_amount) → i128` | Lending integration helper that reverts on manipulated prices and returns collateral value when safe |

---

### Concentrated Liquidity Contract

Located in [contracts/concentrated_liquidity/src/lib.rs](contracts/concentrated_liquidity/src/lib.rs).

A V3-style tick-based AMM where liquidity providers specify a price range `[lower_tick, upper_tick]` for their capital. Only liquidity within the active price range earns fees, which allows far greater capital efficiency than a full-range V2 pool.

**Status: in active development.** The position model and fee accounting are implemented. The tick registry, tick bitmap, sqrtPriceX96 math library, and swap engine are tracked in issues [#177](https://github.com/promisszn/soroban-amm/issues/177)–[#180](https://github.com/promisszn/soroban-amm/issues/180).

#### How it differs from V2

| | V2 AMM | Concentrated Liquidity |
|---|---|---|
| Price model | Full range `x*y=k` | Tick-bounded range positions |
| LP representation | Fungible LP tokens | Per-user tick-range positions |
| Capital efficiency | Liquidity spread over all prices | Capital concentrated in active range |
| Fee accrual | All LPs share fees equally | Only in-range LPs earn fees |
| TWAP | Price ratio accumulator | Tick accumulator (`tick * Δt`) |

#### Public Interface

| Function | Description |
|---|---|
| `initialize(token_a, token_b, fee_bps, initial_tick)` | One-time pool setup |
| `mint_position(provider, lower_tick, upper_tick, amount_a_desired, amount_b_desired, min_a, min_b) → (a, b)` | Open or add to a tick-range position |
| `burn_position(provider, lower_tick, upper_tick, liquidity) → (a, b)` | Reduce or close a position and withdraw tokens |
| `collect_fees(provider, lower_tick, upper_tick) → (a, b)` | Collect accrued fees for a position |
| `get_position(provider, lower_tick, upper_tick) → Position` | Read a position's current state |
| `current_tick() → i32` | Read the active tick |
| `active_liquidity() → i128` | Read total liquidity at the current price |

#### Notes

- Positions are identified by `(owner, lower_tick, upper_tick)` — not by a fungible token. Each address can hold multiple non-overlapping or overlapping positions.
- Depositing a single token is the natural behavior: if the current price is above the position range, only token B is needed; if below, only token A.
- Tick spacing: ticks range from `−887_272` to `887_272`, corresponding to the price range `[~1.0001^−887272, ~1.0001^887272]`.

---

#### Flash Loan Receiver Interface

Borrowers must implement a callback contract with this interface:

```rust
pub trait FlashLoanReceiver {
    fn on_flash_loan(env: Env, token: Address, amount: i128, fee: i128, data: Bytes) -> bool;
}
```

During `flash_loan`, the AMM transfers `amount` of `token` to `receiver`, invokes `on_flash_loan`, and then checks that the pool's token balance increased by at least `fee`. If the receiver does not return `amount + fee` before the callback finishes, the transaction reverts.

### LP Token Contract

Located in [contracts/token/src/lib.rs](contracts/token/src/lib.rs).

| Function | Description |
|---|---|
| `initialize(admin, name, symbol, decimals)` | One-time token setup |
| `mint(to, amount)` | Mint tokens — admin only |
| `burn(from, amount)` | Burn tokens — admin only |
| `transfer(from, to, amount)` | Transfer between accounts |
| `transfer_from(spender, from, to, amount)` | Spend an approved allowance |
| `approve(from, spender, amount)` | Approve a spender |
| `balance(id) → i128` | Read account balance |
| `allowance(from, spender) → i128` | Read spending allowance |
| `total_supply() → i128` | Read total tokens minted |

---

## Math & Formulas

### Constant-Product Invariant (V2)

Every swap must satisfy:

```
reserve_a * reserve_b = k   (constant)
```

### Swap Output

Fees are deducted from the input before applying the formula:

```
amount_in_with_fee = amount_in * (10_000 - fee_bps)

amount_out = (amount_in_with_fee * reserve_out)
           / (reserve_in * 10_000 + amount_in_with_fee)
```

### Initial LP Shares (First Deposit)

Uses the geometric mean of the deposited amounts:

```
shares = sqrt(amount_a * amount_b)
```

### Subsequent LP Shares

Uses the lesser of the two proportional contributions to prevent imbalanced deposits:

```
shares = min(
    amount_a * total_shares / reserve_a,
    amount_b * total_shares / reserve_b
)
```

### Liquidity Removal

Proportional to pool ownership at the time of withdrawal:

```
out_a = shares * reserve_a / total_shares
out_b = shares * reserve_b / total_shares
```

### Concentrated Liquidity Price Model

Price is represented as `sqrtPrice` — the square root of the token B / token A ratio. Ticks are integer indices where each tick step is a `0.01%` price change:

```
price(tick) = 1.0001^tick
```

Token amounts for a position `[lower_tick, upper_tick]` with liquidity `L` are derived from the sqrt price at each boundary, following the Uniswap v3 whitepaper formulas.

---

## Getting Started

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (stable toolchain)
- `wasm32v1-none` compilation target:
  ```sh
  rustup target add wasm32v1-none
  ```
- [Stellar CLI](https://developers.stellar.org/docs/tools/stellar-cli) (`stellar`) for deployment:
  ```sh
  cargo install --locked stellar-cli --features opt
  ```

### Setup

1. **Clone the repository:**

   ```sh
   git clone https://github.com/promisszn/soroban-amm.git
   cd soroban-amm
   ```

2. **Verify the toolchain and target are installed:**

   ```sh
   rustup show                          # confirm stable toolchain is active
   rustup target list --installed       # should include wasm32v1-none
   ```

   If the WASM target is missing:

   ```sh
   rustup target add wasm32v1-none
   ```

3. **Configure the Stellar CLI for your target network** (testnet shown):

   ```sh
   stellar network add testnet \
     --rpc-url https://soroban-testnet.stellar.org \
     --network-passphrase "Test SDF Network ; September 2015"
   ```

4. **Create or import an account identity:**

   ```sh
   # Generate a new keypair and fund it via Friendbot
   stellar keys generate --default-seed mykey
   stellar keys fund mykey --network testnet
   ```

   Or import an existing secret key:

   ```sh
   stellar keys add mykey --secret-key
   # paste your secret key when prompted
   ```

5. **Confirm everything is wired up:**

   ```sh
   stellar keys address mykey           # should print your public key
   ```

You are now ready to build, test, and deploy.

### Build

Build all contracts as optimised WASM binaries:

```sh
cargo build --release --target wasm32v1-none
```

Or via the Makefile alias:

```sh
make build
```

To optimize the compiled WASM binaries for size (typically reducing size by 20-40% using `wasm-opt` through Stellar CLI):

```sh
make optimize
```

Output files:

```
target/wasm32v1-none/release/amm.wasm
target/wasm32v1-none/release/token.wasm
target/wasm32v1-none/release/factory.wasm
target/wasm32v1-none/release/governance.wasm
target/wasm32v1-none/release/twap_consumer.wasm
target/wasm32v1-none/release/concentrated_liquidity.wasm
```

### Test

Run the full test suite across all packages:

```sh
cargo build --release --target wasm32v1-none
cargo test --workspace
```

The factory tests embed compiled WASM at compile time, so the build step is required before running tests. All other packages can be tested independently without a prior build.

The same command runs in CI on every pull request.

For a real-network smoke test on Stellar testnet, run the end-to-end script:

```sh
scripts/e2e.sh
```

The script deploys fresh contracts, funds a test account, adds liquidity, swaps, removes liquidity, and exits non-zero on any failed assertion.

---

## Usage

### Automated Deployment

The fastest way to deploy a full AMM environment (Token A, Token B, LP Token, and AMM Pool) to testnet is using the provided deployment script:

```sh
./scripts/deploy.sh [network]
```

- **network**: Optional target network (defaults to `testnet`).
- The script builds contracts, generates/funds a deployer account, deploys all contracts, and initialises them.
- Deployed contract IDs are printed to the console and saved to `.soroban-amm.deploy.env`.

### ABI Schema

A machine-readable JSON schema of all public contract functions, parameters, and events is available at [docs/abi.json](docs/abi.json).

#### AMM Event Payloads

| Event | Topics | Data Payload |
|---|---|---|
| `swap` | `("swap", trader)` | `(token_in, amount_in, token_out, amount_out)` |
| `add_liquidity` | `("add_liq")` | `(provider, amount_a, amount_b, shares)` |
| `remove_liquidity` | `("rm_liq")` | `(provider, shares, amount_a, amount_b)` |
| `withdraw_fees` | `("wd_fees", fee_recipient)` | `(fee_a, fee_b)` |
| `admin_nominated` | `("admin_nominated")` | `(current_admin, new_admin)` |
| `admin_changed` | `("admin_changed")` | `(new_admin,)` |

#### Governance Event Payloads

| Event | Topics | Data Payload |
|---|---|---|
| `proposed` | `("proposed")` | `(proposal_id, proposer, ProposalKind, vote_end)` |
| `voted` | `("voted")` | `(proposal_id, voter, support, voting_power)` |
| `executed` | `("executed")` | `(proposal_id, ProposalKind)` |
| `cancelled` | `("cancelled")` | `(proposal_id, proposer)` |

#### Concentrated Liquidity Event Payloads

| Event | Topics | Data Payload |
|---|---|---|
| `mint_pos` | `("mint_pos", provider)` | `(lower_tick, upper_tick, liquidity, amount_a, amount_b)` |
| `burn_pos` | `("burn_pos", provider)` | `(lower_tick, upper_tick, liquidity, amount_a, amount_b)` |

### Development

The project includes a `Makefile` to simplify common development tasks:

- `make build`: Build contracts for production (`wasm32v1-none`)
- `make test`: Build WASM then run all contract unit tests
- `make fmt`: Format code using `cargo fmt`
- `make lint`: Run `clippy` with warnings treated as errors
- `make check`: Run formatting, linting, and tests in sequence
- `make deploy`: Deploy contracts to testnet via `scripts/deploy.sh`
- `make e2e`: Run full end-to-end integration tests
- `make clean`: Remove build artifacts

### Reproducible Builds with Docker

To ensure identical WASM binaries across different environments, you can use the provided Docker configuration:

```sh
# Build using Docker Compose
docker compose run --rm build

# Alternatively, using raw Docker
docker build -t soroban-amm-build .
docker run --rm -v $(pwd):/app soroban-amm-build
```

- **Base Image**: `rust:1.93.0-slim`
- **Stellar CLI**: `25.1.0`

### Deploy via Factory

The factory is the recommended way to create pools. It deploys and initialises the AMM pool and its LP token in a single transaction, and registers the pool in its on-chain registry.

**1. Upload the contract WASM blobs:**

```sh
stellar contract upload \
  --wasm target/wasm32v1-none/release/amm.wasm \
  --network testnet --source <YOUR_KEY>
# → prints AMM_WASM_HASH

stellar contract upload \
  --wasm target/wasm32v1-none/release/token.wasm \
  --network testnet --source <YOUR_KEY>
# → prints TOKEN_WASM_HASH
```

**2. Deploy the factory:**

```sh
stellar contract deploy \
  --wasm target/wasm32v1-none/release/factory.wasm \
  --network testnet --source <YOUR_KEY>
# → prints FACTORY_CONTRACT_ID
```

**3. Initialise the factory:**

```sh
stellar contract invoke \
  --id <FACTORY_CONTRACT_ID> \
  --network testnet --source <YOUR_KEY> \
  -- initialize \
  --admin <YOUR_ADDRESS> \
  --amm_wasm_hash <AMM_WASM_HASH> \
  --token_wasm_hash <TOKEN_WASM_HASH>
```

**4. Create a pool (deploys AMM + LP token, registers the pair):**

```sh
stellar contract invoke \
  --id <FACTORY_CONTRACT_ID> \
  --network testnet --source <YOUR_KEY> \
  -- create_pool \
  --token_a <TOKEN_A_CONTRACT_ID> \
  --token_b <TOKEN_B_CONTRACT_ID> \
  --fee_bps 30
# → prints the new POOL_CONTRACT_ID
```

**5. Look up an existing pool:**

```sh
stellar contract invoke \
  --id <FACTORY_CONTRACT_ID> \
  -- get_pool \
  --token_a <TOKEN_A_CONTRACT_ID> \
  --token_b <TOKEN_B_CONTRACT_ID>

stellar contract invoke --id <FACTORY_CONTRACT_ID> -- all_pools
```

---

### Deploy Manually

Deploy the LP token contract first, then the AMM pool. The AMM contract address becomes the LP token's admin.

```sh
# Deploy the LP token
stellar contract deploy \
  --wasm target/wasm32v1-none/release/token.wasm \
  --network testnet \
  --source <YOUR_KEY>

# Deploy the AMM pool
stellar contract deploy \
  --wasm target/wasm32v1-none/release/amm.wasm \
  --network testnet \
  --source <YOUR_KEY>
```

Initialize the LP token (admin = AMM contract address):

```sh
stellar contract invoke \
  --id <LP_TOKEN_CONTRACT_ID> \
  --network testnet \
  --source <YOUR_KEY> \
  -- initialize \
  --admin <AMM_CONTRACT_ID> \
  --name "Pool LP Token" \
  --symbol "AMMLP" \
  --decimals 7
```

Initialize the AMM pool (fee of 30 bps = 0.30%):

```sh
stellar contract invoke \
  --id <AMM_CONTRACT_ID> \
  --network testnet \
  --source <YOUR_KEY> \
  -- initialize \
  --token_a <TOKEN_A_CONTRACT_ID> \
  --token_b <TOKEN_B_CONTRACT_ID> \
  --lp_token <LP_TOKEN_CONTRACT_ID> \
  --fee_bps 30 \
  --fee_recipient <FEE_RECIPIENT_ADDRESS> \
  --protocol_fee_bps 0
```

### Add Liquidity

```sh
stellar contract invoke \
  --id <AMM_CONTRACT_ID> \
  --network testnet \
  --source <YOUR_KEY> \
  -- add_liquidity \
  --provider <PROVIDER_ADDRESS> \
  --amount_a 1000000 \
  --amount_b 2000000 \
  --min_shares 0 \
  --deadline <UNIX_TIMESTAMP>
```

`min_shares` is the minimum LP tokens you are willing to accept. Set to `0` to skip slippage protection during initial seeding. `deadline` is the latest ledger timestamp at which the call is valid.

### Swap Tokens

```sh
stellar contract invoke \
  --id <AMM_CONTRACT_ID> \
  --network testnet \
  --source <YOUR_KEY> \
  -- swap \
  --trader <TRADER_ADDRESS> \
  --token_in <TOKEN_A_CONTRACT_ID> \
  --amount_in 100000 \
  --min_out 0 \
  --deadline <UNIX_TIMESTAMP>
```

Use `get_amount_out` first to compute an appropriate `min_out`.

### Remove Liquidity

```sh
stellar contract invoke \
  --id <AMM_CONTRACT_ID> \
  --network testnet \
  --source <YOUR_KEY> \
  -- remove_liquidity \
  --provider <PROVIDER_ADDRESS> \
  --shares <LP_SHARE_AMOUNT> \
  --min_a 0 \
  --min_b 0 \
  --deadline <UNIX_TIMESTAMP>
```

### Query the Pool

```sh
# Full pool info
stellar contract invoke --id <AMM_CONTRACT_ID> -- get_info

# Quote a swap
stellar contract invoke --id <AMM_CONTRACT_ID> \
  -- get_amount_out \
  --token_in <TOKEN_A_CONTRACT_ID> \
  --amount_in 100000

# LP share balance
stellar contract invoke --id <AMM_CONTRACT_ID> \
  -- shares_of --provider <PROVIDER_ADDRESS>
```

### Use the TWAP Oracle

The AMM exposes cumulative price state with `get_price_cumulative()`. The example consumer contract shows one way to turn that into a fixed-window TWAP.

1. Deploy `twap_consumer.wasm`.
2. Save a snapshot (for example every minute):

```sh
stellar contract invoke \
  --id <TWAP_CONSUMER_CONTRACT_ID> \
  --network testnet --source <YOUR_KEY> \
  -- save_snapshot \
  --pool <AMM_CONTRACT_ID>
```

3. After `window_seconds` has elapsed, read TWAP:

```sh
stellar contract invoke \
  --id <TWAP_CONSUMER_CONTRACT_ID> \
  --network testnet --source <YOUR_KEY> \
  -- get_twap_price \
  --pool <AMM_CONTRACT_ID> \
  --window_seconds 60
```

4. Validate the real-time AMM spot price against TWAP before accepting it in another protocol:

```sh
stellar contract invoke \
  --id <TWAP_CONSUMER_CONTRACT_ID> \
  --network testnet --source <YOUR_KEY> \
  -- validate_price_against_twap \
  --pool <AMM_CONTRACT_ID> \
  --window_seconds 60 \
  --spot_price <AMM_PRICE_RATIO_A> \
  --max_deviation_bps 500
```

Lending contracts can call `assert_lending_price_safe` before valuing collateral. The helper reverts when the real-time spot price differs from TWAP by more than `max_deviation_bps`, preventing a flash-loan-moved spot price from being used as the collateral oracle.

Notes:

- `window_seconds` must be greater than 0.
- `save_snapshot` must have been called at approximately `now_ts - window_seconds`.
- Returned TWAP is scaled the same way as AMM spot price (`1_000_000` scale factor).
- `max_deviation_bps` is configurable per integration; for example, `500` allows a 5% spot/TWAP difference.

### TypeScript Client Example

A standalone TypeScript client is available in [examples/client](examples/client). It demonstrates connecting to Stellar testnet RPC, reading `get_info()`, quoting with `get_amount_out()`, executing `swap()`, and reading LP shares with `shares_of()`.

```sh
cd examples/client
npm install
npm run build
npm start
```

### Python Client Example

A standalone Python client is available in [examples/python](examples/python). It demonstrates the same flow using `py-stellar-base` (`stellar-sdk`): connect to Stellar testnet RPC, read `get_info()`, quote with `get_amount_out()`, execute `swap()`, and read LP shares with `shares_of()`.

```sh
cd examples/python
python3 -m venv .venv
. .venv/bin/activate
pip install -r requirements.txt
python client.py
```

---

## Contributing

Contributions are welcome. Please follow the guidelines below to keep the codebase consistent and review cycles short.

### Reporting Issues

- Search existing issues before opening a new one.
- Include the Rust / `soroban-sdk` version, the steps to reproduce, and the expected vs. actual behavior.
- For security vulnerabilities, **do not open a public issue** — see [SECURITY.md](SECURITY.md) for the responsible disclosure process.

### Development Workflow

1. **Fork** the repository and create a branch from `main`:

   ```sh
   git checkout -b feat/my-feature
   ```

   Branch naming conventions:
   | Prefix | Use for |
   |---|---|
   | `feat/` | New features |
   | `fix/` | Bug fixes |
   | `refactor/` | Code restructuring without behavior change |
   | `test/` | Adding or improving tests |
   | `docs/` | Documentation only |
   | `chore/` | Build scripts, tooling, dependencies |

2. **Make your changes**, then ensure the build and tests pass:

   ```sh
   cargo build --release --target wasm32v1-none
   cargo test --workspace
   ```

3. **Write tests** for any new behavior. All public functions should have at least one test. Tests live alongside the implementation in `src/lib.rs` under a `#[cfg(test)]` module.

4. **Keep commits focused.** One logical change per commit. Use the [Conventional Commits](https://www.conventionalcommits.org/) format:

   ```
   feat: add time-weighted average price accumulator
   fix: prevent zero-share mint on initial deposit
   test: cover swap with maximum fee setting
   ```

5. **Open a Pull Request** against `main`. In the PR description:
   - Explain _what_ changed and _why_.
   - Reference any related issues with `Closes #<issue>` or `Related to #<issue>`.
   - If the change affects contract behavior, include before/after output or test coverage evidence.

### Code Style

- An [`.editorconfig`](.editorconfig) at the workspace root defines shared formatting rules (UTF-8, LF line endings, 4-space indentation, trailing-whitespace trimming). Most editors apply it automatically; install the [EditorConfig plugin](https://editorconfig.org/#download) if yours does not.
- A [`rustfmt.toml`](rustfmt.toml) at the workspace root defines Rust formatting rules. It enforces:
  - **Edition**: 2021
  - **Max width**: 100 columns
  - **Indentation**: 4 spaces
  - **Line endings**: Unix (LF)
  - **Import grouping**: Standard library, external crates, then crate-local modules
- Run `cargo fmt` before committing to automatically apply these rules.
- Run `cargo clippy -- -D warnings` and resolve any warnings before opening a PR.
- Prefer explicit arithmetic with overflow checks over silent wrapping. The release profile already enables `overflow-checks = true`.
- Avoid unsafe code. There is no reason to use `unsafe` in a Soroban contract.
- Do not add dependencies without discussion. The contract binary size and attack surface matter.

### Pull Request Checklist

Before requesting review, confirm:

- [ ] `cargo fmt` has been run
- [ ] `cargo clippy -- -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] New behavior is covered by tests
- [ ] Public interface changes are reflected in this README
- [ ] `CHANGELOG.md` has been updated with any notable changes
- [ ] Commit messages follow the Conventional Commits format

### Versioning

This project follows [Semantic Versioning](https://semver.org/). Breaking changes to the on-chain interface (function signatures, storage layout, error codes) constitute a major version bump.

---

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for a history of notable changes to this project.

---

## Security

Please do not open public issues for security vulnerabilities. See [SECURITY.md](SECURITY.md) for the full vulnerability disclosure policy, supported versions, and how to reach the maintainers privately.

---

## License

This project is licensed under the [MIT License](LICENSE).
