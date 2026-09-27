# Deployment Runbook — Soroban AMM Protocol

> Operational document for deploying, verifying, upgrading, and recovering the
> full Soroban AMM protocol. An operator who has never seen the repo can follow
> this end to end. For contract-level invariants and math, see `README.md` and
> `docs/pool-management-guide.md`.

---

## Table of Contents

1. [Prerequisites](#1-prerequisites)
2. [Deployment Order & Dependency Reasoning](#2-deployment-order--dependency-reasoning)
3. [Per-Contract Initialization Parameters](#3-per-contract-initialization-parameters)
4. [Post-Deployment Verification](#4-post-deployment-verification)
5. [Upgrade Procedure](#5-upgrade-procedure)
6. [Emergency Procedures](#6-emergency-procedures)
7. [Admin Key Management](#7-admin-key-management)
8. [Network-Specific Notes](#8-network-specific-notes)
9. [Failure Modes & Error Codes](#9-failure-modes--error-codes)
10. [Deployment Script Reference](#10-deployment-script-reference)

---

## 1. Prerequisites

### 1.1 Toolchain

| Tool | Required version | Check | Install |
|------|-----------------|-------|---------|
| Rust | stable ≥ 1.75 | `rustc --version` | `rustup update` |
| WASM target | `wasm32v1-none` | `rustup target list --installed \| grep wasm32v1-none` | `rustup target add wasm32v1-none` |
| Stellar CLI | **25.1.0** (pinned) | `stellar --version` | `cargo install --locked stellar-cli@25.1.0 --features opt` |
| Docker (optional) | any recent | `docker --version` | For reproducible builds: `docker compose run --rm build` |

> The WASM target **must** be `wasm32v1-none`. The legacy `wasm32-unknown-unknown`
> target produces binaries for the wrong Soroban environment and will fail on
> upload or behave incorrectly. `scripts/deploy.sh` enforces this — it builds
> with `--target wasm32v1-none` and warns if stale `wasm32-unknown-unknown`
> artifacts are present.

The pinned Stellar CLI version (25.1.0) is the version used in CI and in the
`Dockerfile` (`rust:1.93.0-slim` base). Newer CLI versions may change `stellar
contract upload` / `invoke` flag names — pin to avoid silent breakage.

Recommended `rustc`/`cargo` versions are in `rust-toolchain.toml` if present;
otherwise use the latest stable. The workspace `Cargo.toml` sets
`soroban-sdk = 21.7.7`.

### 1.2 Network Configuration

Add RPC endpoints once (examples show testnet):

```sh
stellar network add testnet \
  --rpc-url https://soroban-testnet.stellar.org \
  --network-passphrase "Test SDF Network ; September 2015"

stellar network add mainnet \
  --rpc-url https://mainnet.stellar.validationcloud.io \
  --network-passphrase "Public Global Stellar Network ; September 2015"
```

The deploy script reads `NETWORK` (or `STELLAR_NETWORK`) and passes it as
`--network`. Valid values: `testnet`, `mainnet`, `futurenet`, `local`.

### 1.3 Account Funding

The deployer account (`--source`) must exist and hold enough XLM for
contract deploys (≈ 1–2 XLM per contract on testnet; more on mainnet due to
storage rent).

Create and fund on testnet:

```sh
stellar keys generate --default-seed soroban-amm-deployer
stellar keys fund soroban-amm-deployer --network testnet
stellar keys address soroban-amm-deployer  # save this public key
```

On mainnet, fund by sending XLM from an exchange or funded wallet — Friendbot
does not exist on mainnet. Ensure the account has a trustline/pinned funding
before running `scripts/deploy.sh`.

Set:

```sh
export SOURCE_ACCOUNT=soroban-amm-deployer
export NETWORK=testnet          # or mainnet
# Optionally override admin/fee recipient:
export ADMIN_ADDRESS=G...       # defaults to SOURCE_PUBLIC_KEY
export FEE_RECIPIENT=G...
```

The script will generate and fund the source key automatically if it does not
exist (testnet only).

### 1.4 Build Before Deploy

```sh
cargo build --release --target wasm32v1-none
# Or via Make:
make build
# To shrink binaries 20–40% for upload limits:
make optimize
```

The deploy script also builds automatically if any expected
`target/wasm32v1-none/release/*.wasm` file is missing, but pre-building lets
you verify the build succeeds before touching the network.

Expected artifacts (18 deployable crates):

```
amm.wasm
token.wasm
factory.wasm
concentrated_liquidity.wasm
governance.wasm
staking.wasm
twap_consumer.wasm
twal_consumer.wasm
oracle_aggregator.wasm
router.wasm
batch_router.wasm
dex_aggregator.wasm
batch_auction.wasm
cl_position_nft.wasm
reserve_manager.wasm
incentive_campaigns.wasm
pol_vesting.wasm
v2_to_v3_migration.wasm
```

(`amm-sdk` and `amm-fuzz` are libraries, not deployable.)

---

## 2. Deployment Order & Dependency Reasoning

The protocol is a DAG — each contract's `initialize` takes addresses of
contracts that must already exist and be initialized. Deploying out of order
fails with `NotInitialized` or missing-hash errors.

```
token (assets) ──────────┐
                         ▼
          WASM uploads (token, amm, cl) ──► factory ──► pools (AMM + CL) ──► governance ──┐
                                                                │                          │
                                                                ├──► CL pool ──► cl_position_nft
                                                                │
                    oracle_aggregator (standalone, early — pools may wire it later) ───────────┤
                    twap_consumer ──► needs pool + keeper                                      │
                    twal_consumer ──► needs pool + keeper                                      │
                                                                │
governance ──► staking (needs LP token + reward token) ──────────┤
       ├──► incentive_campaigns (needs governance)                │
       ├──► pol_vesting (needs governance + treasury)             │
       └──► reserve_manager (needs governance + factory)          │
                                                                │
factory ──► router, batch_router, dex_aggregator (needs factory)  │
admin ──► batch_auction (needs admin + window)                    │
pools ──► v2_to_v3_migration (needs both V2 and V3 pool)          │
```

**Step-by-step table:**

| # | Contract(s) | Depends on | Why this order |
|---|-------------|------------|----------------|
| 1 | **Token A, Token B, Reward Token** | None | Standalone SEP-41 tokens. All pools and staking need them as underlying assets. Created first so factory pools can reference them. |
| 2 | **WASM uploads** (`token.wasm`, `amm.wasm`, `concentrated_liquidity.wasm`) | Tokens built | Factory's `initialize` requires pre-uploaded WASM hashes (`amm_wasm_hash`, `token_wasm_hash`). Hashes are 32-byte SHA-256 of the WASM; they must be `stellar contract upload`ed before factory init. CL hash is registered afterward via `set_cl_wasm_hash`. |
| 3 | **Factory** | WASM hashes + admin | Deploys and owns pool creation. Single entry point for V2 and CL pools. Sets `DefaultFeeTier=2` (30 bps). Must exist before any pool. |
| 4 | **Pools via Factory** (`create_pool`, `create_cl_pool`) | Factory + Token A/B | Factory deploys LP token (admin = pool) and AMM pool in one tx, enforces pair uniqueness, registers in `Pool(token_a,token_b) → pool`. CL pool needs `initial_tick` and `tick_spacing` derived from fee tier. Pools are **never** deployed directly — the factory path guarantees `Pool → LpToken` linkage and indexing for `dex_aggregator`. |
| 5 | **Governance** | AMM pool + LP token | LP-weighted voting: `balance_at` snapshots need the LP token's checkpoint history. Governance becomes the LP token's `locker` so `vote` can lock shares during the voting window. Factory wiring: if pools were created with a `governance_wasm_hash`, factory already deployed governance; otherwise deploy here. |
| 6 | **Oracle Aggregator** | Admin only | Standalone median-price aggregator over fresh, agreeing sources. No pool dependency for deployment, but pools can later `set_oracle` to wire it. Deploy early so pools can be configured with it immediately after. |
| 7 | **TWAP Consumer / TWAL Consumer** | Keeper (admin) + pools (optional) | Read `get_price_cumulative` / `get_liquidity_cumulative` from pools and store snapshots. Initialized with a `keeper` address authorized to call `save_snapshot`. No pool required at init — keeper can start snapshotting after. |
| 8 | **Staking** | LP token + Reward token + admin | Users stake LP tokens for reward-token emissions. Boost-lock config (`min_boost=1x`, `max_boost=2.5x`, `min_lock=7d`, `max_lock=4y`) is set at init. Needs LP token address to transfer stakes. |
| 9 | **Incentive Campaigns** | Governance | Governance creates time-based campaigns with `reward_rate * duration <= funding`. Needs governance address so only governance can call `create_campaign`. |
| 10 | **POL Vesting** | Governance + Treasury | Linear vesting of POL LP tokens between `cliff_ledger` and `end_ledger`. Governance creates/revokes, treasury receives revoked tokens. |
| 11 | **Reserve Manager** | Governance + Factory | Off-chain gate `check_reserves(pool)` — reads `get_info()` and compares to `min_reserve` per pair. No AMM hook (see issue #518); bots/dashboards call it before migration. |
| 12 | **Router / Batch Router** | Factory | Multi-hop `swap_exact_in` across pools discovered via `factory.get_pool`. Atomic batch of swaps/liquidity ops. |
| 13 | **DEX Aggregator** | Factory + Admin + CL pools | Cross-venue best-execution router over AMM + CL pools. Initialized with `MaxHops=4`, `MAX_CL_POOLS=50`, `CL_FEE_TIERS=[30,100,500]`. CL pools must be `register_cl_pool`ed before they participate in routing. |
| 14 | **Batch Auction** | Admin + `batch_window_secs` | Collects orders for `batch_window_secs` then `settle_batch` atomically. Needs no pool at init — validates `pool_matches_pair` at `submit_order`. |
| 15 | **CL Position NFT** | CL pool | ERC-721 receipt for CL positions. Only `cl_pool` may `mint`/`burn`. After deploy, `cl_pool.set_position_nft(nft)` wires it so positions automatically mint an NFT. |
| 16 | **V2 → V3 Migration** | V2 pool + V3 pool + admin | Burns V2 LP shares and mints a CL position in one tx. Verifies `token_a/token_b` match or reverts `TokenMismatch`. |

Within `scripts/deploy.sh` these steps are executed by `deploy_tokens`, `deploy_factory`,
`deploy_pools`, `deploy_governance`, etc., each respecting `--only`/`--skip`.

---

## 3. Per-Contract Initialization Parameters

Every parameter lists: **what it is**, **recommended value**, **rationale**, and
**what happens if it is wrong or out of bounds**.

### 3.1 Token (`token`)

| Param | Type | Recommended | Rationale | If wrong |
|-------|------|-------------|-----------|----------|
| `admin` | Address | AMM pool address (when used as LP token) or deployer for asset tokens | Only `admin` can `mint`/`burn`. For LP tokens, the pool must be admin so `add_liquidity` can mint shares. | If asset token admin = pool, anyone adding liquidity could inflate supply. If LP token admin ≠ pool, `add_liquidity` reverts. |
| `name` | String | `"AMM LP Token #N"` (auto) / `"Soroban AMM Token A"` | Human-readable; no on-chain effect. | None, but confusing UX. |
| `symbol` | String | `"ALP0"` / `"SAMA"` | DEX display; max ~12 chars recommended. | None. |
| `decimals` | u32 | `7` (Stellar default) | Matches SEP-41 and most Stellar assets. | Swaps/amounts mis-scaled by 10^(decError). |

**Storage lock / checkpoints:** `MAX_CHECKPOINTS=1024`, TTL `MIN_TTL=120960` / `BUMP_TO=2419200`. No init param — baked in. Governance snapshots query `balance_at(ledger)`; if history is truncated, queries before retained history revert rather than returning a bogus 0.

### 3.2 AMM Pool (`amm`) — created via `factory.create_pool`

Pools are not initialized directly in the full-protocol flow. Factory's
`create_pool` calls `amm.initialize` with:

| Param | Recommended | Rationale | If wrong |
|-------|-------------|-----------|----------|
| `admin` | `gov` if governance deployed, else factory `admin` | Governance-administered pools allow on-chain fee changes; factory-admin pools are upgradable centrally. | Wrong admin = fees/upgrades paused or controlled by wrong key. |
| `token_a/b` | `TOKEN_A_CONTRACT_ID`, `TOKEN_B_CONTRACT_ID` (normalized order) | Pair uniqueness is enforced by factory. | Duplicate pool reverts `PoolAlreadyExists (3)`. |
| `lp_token` | Factory-deployed LP token (auto) | Factory generates salt `n*3` and `n*3+1` deterministically. | Manual LP token breaks factory `get_lp_token` lookup and `dex_aggregator` discovery. |
| `fee_bps` | `30` (0.30% = Medium/fee_tier 2) | See fee-tier table below. 0.30% is the Uniswap-v2 default, good for most pairs. Stablecoin pairs may prefer `1` or `5`. | `>10000` reverts `InvalidFeeBps (2)`. `0` = no fees (LPs earn nothing). `>=10000` undercuts all LP incentive. |
| `fee_recipient` | Factory contract address | Factory becomes `fee_recipient` so `sweep_fees` can collect protocol fees. | Fees accrue to wrong address; `sweep_fees` no-ops. |
| `protocol_fee_bps` | `0` (disabled at deploy) | Must be `< fee_bps` (LPs must retain some of each swap). Enabling later via `set_protocol_fee` is safe after governance review. | `protocol_fee_bps >= fee_bps` reverts `InvalidFeeBps`. `protocol_fee_bps == fee_bps` would route 100% to protocol, starving LPs (guarded by M-02 fix). |
| `flash_loan_fee_bps` | Defaults to `fee_bps` if using `initialize`; custom via `initialize_with_flash_loan_fee` | Flash-loan fee may be lower than swap fee for comptime arbitrage, but should not be 0 or under-collateralized borrows are free. | Diverging too low = underpriced borrowing risk. Too high = arbitrage unprofitable, reduces volume. |

**Fee tiers (factory):**

| Tier | `fee_tier` | `fee_bps` | Use |
|------|-----------|-----------|-----|
| VeryLow | 0 | 1 (0.01%) | Stablecoin/stablecoin (like USDC/USDT) |
| Low | 1 | 5 (0.05%) | Correlated pairs |
| **Medium** | **2** | **30 (0.30%)** | **Volatile pairs (default)** |
| High | 3 | 100 (1.00%) | Exotic/illiquid pairs |

**AMM defaults (not init params but tuned via setters):**

| Setting | Default | Recommended | Notes |
|---------|---------|-------------|-------|
| `LpRebateBps` | 0 | 0–5000 (0–50% of protocol fee back to LPs) | Set via `set_lp_rebate`. 5000 = half the protocol cut is returned to reserves, softening LP loss. |
| `CircuitBreakerThresholdBps` | 5000 (50%) | 5000 (testnet) / 3000–5000 (mainnet) | Spot price deviation in one ledger that auto-pauses the pool. Lower = safer but more false positives on volatile pairs. |
| `CircuitBreakerCooldown` | 600s (10 min) | 600–1800s | Must elapse before `try_circuit_breaker_recovery`. Shorter = faster recovery, longer = more time for governance review. |
| `MaxOracleDeviationBps` | 500 (5%) | 500 | Spot vs. oracle price check in `swap`. Disabled until `set_oracle` is called. |

### 3.3 Concentrated Liquidity (`concentrated_liquidity`) — via `factory.create_cl_pool`

| Param | Recommended | Rationale | If wrong |
|-------|-------------|-----------|----------|
| `admin` | `ADMIN_ADDRESS` | Governance or multisig for `pause`/`set_protocol_fee`/`upgrade`. | Central admin can pause CL pool; wrong key = denial-of-service. |
| `token_a/b` | Same as V2 pair | Allows parallel V2 and V3 liquidity on same pair at different fee tiers. Triplet `(token_a, token_b, fee_bps)` is unique. | Duplicate triplet reverts `ClPoolAlreadyExists (4)`. |
| `fee_bps` | `30`, `100`, or `500` (routed tiers) | Factory maps `5→spacing 1`, `30→10`, `100→60`, `500→200`. Custom fees use `1` spacing. | `>10000` reverts `InvalidFeeBps`. Non-tiered values work but may not be routed by `dex_aggregator` (`CL_FEE_TIERS=[30,100,500]`). |
| `initial_tick` | `0` (price 1:1) for a new pair with no price history; else current mid price tick | `tick = log_{1.0001}(price)`. Range `[-887272, 887272]`. | Out of range reverts `TickOutOfRange (4)`. Wrong initial price = immediate arbitrage loss. |
| `tick_spacing` | Derived from fee (see above) | Enforced — ticks must be multiples of spacing, else `TickNotAligned (13)`. | Wrong spacing = mispriced ranges and wasted gas on invalid positions. |

### 3.4 Factory (`factory`)

| Param | Recommended | Notes |
|-------|-------------|-------|
| `admin` | `ADMIN_ADDRESS` (deployer or multisig) | Becomes the `fee_recipient` for all factory-created AMM pools and authorizes `upgrade`, `set_cl_wasm_hash`, `set_treasury`, etc. |
| `amm_wasm_hash` | SHA-256 of `amm.wasm` (uploaded) | Must be uploaded via `stellar contract upload` before `initialize`. Changing it later via `update_wasm_hashes` only affects future pools. |
| `token_wasm_hash` | SHA-256 of `token.wasm` | Same. |
| `cl_wasm_hash` | After init via `set_cl_wasm_hash` | Required before `create_cl_pool`; otherwise reverts `ClWasmNotSet (5)`. |
| `DefaultFeeTier` | `2` (30 bps) | Set automatically by `initialize`; tunable via `set_default_fee_tier(0–3)`. |
| `PermissionlessMode` | `false` initially | When `true`, anyone can `create_pool` by paying `PoolCreationFee` in `FeeToken`, rate-limited by `RateLimitLedgers`. Keep `false` until spam policy is decided. |
| `PoolCreationFee` / `FeeToken` | `0` (unset) | If permissionless, set via `set_pool_creation_fee`. Recommended: 1–10 XLM equivalent to deter spam without deterring legitimate pools. |
| `RateLimitLedgers` | `1` (default) | Minimum ledgers between creations per address. Increase under spam attack. |

### 3.5 Governance (`governance`)

| Param | Recommended | Rationale | If wrong |
|-------|-------------|-----------|----------|
| `admin` | `ADMIN_ADDRESS` | Authorizes `set_min_proposer_stake_bps`, `set_veto_multisig`, etc. | Wrong admin = governance parameter changes controlled by attacker. |
| `amm_pool` | `AMM_POOL_CONTRACT_ID` | Proposal execution calls `amm.update_fee` / `set_protocol_fee`. | Wrong pool = proposals change fees on the wrong market. |
| `lp_token` | `LP_TOKEN_CONTRACT_ID` | Voting power = `balance_at(snapshot_ledger)`. Lock via `lp_token.lock`. | Wrong token = voting power zero or unrelated token, quorum never met. |
| `voting_period_secs` | `604800` (7 days) | 7 days is standard for on-chain governance (enough for discussion, not so long that parameter fixes stall). | `0` or negative rejected `InvalidVotingPeriod (2)`. Too short (<1 day) = voters miss the window; too long (>30 days) = parameters cannot respond to crises. |
| `timelock_secs` | `172800` (2 days) | Delay between `voting ends` and `execute()` — lets users exit before a contentious fee change takes effect. | Too short = no exit window; too long = urgent fixes (e.g. pausing a broken pool) are blocked — use `cancel_proposal` path or direct `pause` instead. |
| `quorum_bps` | `1000` (10%) | Of total LP supply at snapshot. 10% balances liveness (quorum reachable) vs. security (small coalition cannot pass). | `0` or `>10000` rejected `InvalidQuorumBps (4)`. Too low = governance capture; too high = proposals never pass. |
| `min_proposer_stake_bps` | `100` (1%) | Of total LP supply. Prevents spam proposals while letting genuine LPs participate. | Too high = only whales can propose; too low = proposal spam. |
| `veto_multisig` | Unset initially; set via `set_veto_multisig` if needed | Within `VETO_WINDOW_SECS=86400` (24h) after voting ends, the veto multisig can `veto()` a passing proposal. | Without veto, a flash-governance attack cannot be stopped. With an overly powerful veto multisig, governance is centrally controlled — keep threshold ≥ 3-of-5 multisig. |

### 3.6 Staking (`staking`)

| Param | Recommended | Notes |
|-------|-------------|-------|
| `lp_token` | `LP_TOKEN_CONTRACT_ID` | Token users stake. |
| `reward_token` | `REWARD_TOKEN_CONTRACT_ID` | Token distributed via `accumulated_rewards_per_share` (1e18 scale). |
| `admin` | `ADMIN_ADDRESS` | Calls `add_rewards` / `update_rewards`. |
| `min_boost_scaled` | `10000` (1x) | Default; change via `initialize_with_boost_config`. |
| `max_boost_scaled` | `25000` (2.5x) | 2.5x at max lock. Higher (e.g. 4x) centralizes rewards to long lockers. |
| `min_lock_duration_secs` | `604800` (7 days) | Below this, boost = 1x (`stake` path). |
| `max_lock_duration_secs` | `126144000` (4 years) | At this duration, boost = `max_boost_scaled`. Linear interpolation in between. |
| `ConfigMaxRewardPoolBalance` | `0` (no cap) | If set, `claim` stops when pool depleted. |

Rewards are added via `add_rewards(admin, amount)` which `transfer`s reward tokens into the staking contract; use a funding account with enough balance. Circuit breaker `Paused` / `EmergencyMode` are separate from the AMM breaker.

### 3.7 Incentive Campaigns (`incentive_campaigns`)

| Param | Recommended | Notes |
|-------|-------------|-------|
| `governance` | `GOVERNANCE_CONTRACT_ID` | Only governance can `create_campaign`, `set_campaign_rate`. |
| Campaign `reward_rate * duration <= funding_amount` | Enforced | Prevents underfunded campaigns. |
| `PRECISION` | `1e12` (fixed) | Per-second accumulator to prevent flash-deposit theft (fix for #425). |

### 3.8 POL Vesting (`pol_vesting`)

| Param | Recommended | Notes |
|-------|-------------|-------|
| `governance` | `GOVERNANCE_CONTRACT_ID` | Only governance can create/revoke schedules. |
| `treasury` | `ADMIN_ADDRESS` or multisig treasury | Receives tokens when a schedule is `revoke`d. |
| `start_ledger`, `cliff_ledger`, `end_ledger` | `end > cliff >= start` | Linear vesting: `releasable = total * (ledger - cliff) / (end - cliff)` after cliff. Before cliff, `release` returns `NothingToRelease (5)`. |

### 3.9 Reserve Manager (`reserve_manager`)

| Param | Notes |
|-------|-------|
| `governance` | `GOVERNANCE_CONTRACT_ID` |
| `factory` | `FACTORY_CONTRACT_ID` |
| `min_reserve` per pair | Set via `set_min_reserve(token_a,token_b, min_a, min_b)`. Off-chain gate — AMM withdrawals are **not** blocked on-chain (issue #518). Keep `min_reserve` at ~5–10% of expected TVL so dashboards alert before pools drain. |

### 3.10 Oracle Aggregator (`oracle_aggregator`)

| Param | Recommended | Rationale |
|-------|-------------|-----------|
| `admin` | `ADMIN_ADDRESS` | Registers/removes sources via `add_source`/`remove_source`. |
| `max_staleness_seconds` | `3600` (1 hour) | Sources older than this are ignored. For stablecoins, 300s is tighter; for illiquid pairs, 3600s avoids flapping confidence. Rejects `0` (`InvalidStaleness (7)`). |
| `max_deviation_bps` | `500` (5%) | Fresh quotes outside median ±5% are dropped as `deviant` and not counted toward `confidence`. Tighter (e.g. 200) = more sensitive, may reject legitimate cross-venue spread. Looser (e.g. 1000) = attacker can pull median halfway before being flagged. |

Needs `MIN_VALID_SOURCES=2` fresh, agreeing sources to return non-zero confidence.

### 3.11 TWAP / TWAL Consumers

| Param | Recommended | Notes |
|-------|-------------|-------|
| `keeper` | `ADMIN_ADDRESS` | Authorized to `save_snapshot(pool)` / `save_cl_snapshot`. Run a cron (e.g. every 60s) calling `save_snapshot`. |
| `SNAPSHOT_TTL_LEDGERS` | `120960` (~7 days at 5s/ledger) | Snapshots evicted after TTL — keeper must snapshot frequently enough that `get_twap_price(pool, window)` can always find `now_ts - window`. |

TWAP is `(cum_a_now - cum_a_then) / window` scaled by `1_000_000`. CL path uses `get_tick_cumulative`. TWAL differences `active_liquidity * elapsed`.

### 3.12 Router / Batch Router / DEX Aggregator / Batch Auction

| Contract | Param | Recommended | Notes |
|----------|-------|-------------|-------|
| **Router** | `factory` | `FACTORY_CONTRACT_ID` | Discovers pools via `get_pool`. No admin param. |
| **Batch Router** | `factory` | `FACTORY_CONTRACT_ID` | `MAX_BATCH_OPS=200`. No `DeadlineExpired` per leg — overall deadline enforced by caller. |
| **DEX Aggregator** | `admin` | `ADMIN_ADDRESS` | Controls `register_cl_pool`, `set_max_hops`. |
|  | `factory` | `FACTORY_CONTRACT_ID` |  |
|  | `max_hops` | `4` (default) | 4-hop paths are exhaustive enough without excessive gas. |
| **Batch Auction** | `admin` | `ADMIN_ADDRESS` |  |
|  | `batch_window_secs` | `60` | Orders collected for 60s, then `settle_batch` executes atomically. Lower = more latency-sensitive, higher = more orders per batch but slower fills. Max orders default `50`, ceiling `200`. |

### 3.13 CL Position NFT (`cl_position_nft`)

| Param | Recommended | Notes |
|-------|-------------|-------|
| `admin` | `ADMIN_ADDRESS` | Can `set_ttl_params`. |
| `cl_pool` | `CL_POOL_CONTRACT_ID` | Only this pool may `mint`/`burn` NFTs. After init, call `cl_pool.set_position_nft(nft)` to wire it so `mint_position` auto-mints an NFT. |
| TTL | `DEFAULT_MIN_TTL=518400` (~30d), `DEFAULT_BUMP_TO=3110400` (~180d) | Bump on every read/write to prevent position-NFT eviction for long-lived positions. |

### 3.14 V2 → V3 Migration (`v2_to_v3_migration`)

| Param | Notes |
|-------|-------|
| `admin` | `ADMIN_ADDRESS` |
| `v2_pool` | `AMM_POOL_CONTRACT_ID` |
| `v3_pool` | `CL_POOL_CONTRACT_ID` |

Verifies `token_a/token_b` match or reverts `TokenMismatch`. Sentinel ticks `i32::MIN`/`MAX` map to `current_tick ± width` for single-sided migration.

---

## 4. Post-Deployment Verification

A deployment that "succeeded" (zero exit code) but left a contract
uninitialized costs the most to debug later. Run these checks manually or rely
on the script's built-in verification — every `invoke` in the script is
followed by a read-back.

### 4.1 Automated verification (built into `scripts/deploy.sh`)

After each `initialize`, the script reads state and asserts:

| Contract | Verification call | Asserts |
|----------|-------------------|---------|
| Token A/B/Reward | `name`, `total_supply`, `admin` | `admin == $SOURCE_PUBLIC_KEY`, `name` readable |
| Factory | `get_pool_count`, `get_pools` | Hashes registered, count readable; `creation_paused == false` (unless paused) |
| AMM Pool (via factory) | `get_info` | `token_a/b == expected`, `fee_bps == 30`, `total_shares == 0`, `admin == gov or factory_admin` |
| LP Token | `admin` | `admin == AMM_POOL_CONTRACT_ID` |
| CL Pool | `current_tick` / `get_pool_state` | Returns `initial_tick` |
| Governance | `get_params` | `amm_pool == AMM_POOL`, `lp_token == LP_TOKEN`, `voting_period == 604800` |
| LP locker | `locker` on LP token | `locker == GOVERNANCE_CONTRACT_ID` |
| Staking | `get_pool_info` | `lp_token == LP_TOKEN`, `reward_token == REWARD_TOKEN` |
| Oracle Aggregator | `get_sources` / `get_admin` | `admin == ADMIN_ADDRESS` |
| TWAP/TWAL Consumer | `get_keeper` | `keeper == ADMIN_ADDRESS` |
| Router/Batch Router | `get_factory` (if exposed) | `factory == FACTORY_CONTRACT_ID` |
| DEX Aggregator | `get_factory` / `get_admin` | matches |
| Batch Auction | `get_admin` / `get_batch_window` | matches |
| CL Position NFT | `get_admin` / `next_token_id` | `admin == ADMIN_ADDRESS` |
| POL Vesting | `get_governance` | `governance == GOVERNANCE_CONTRACT_ID` |
| Reserve Manager | `get_governance` | `governance == GOVERNANCE_CONTRACT_ID` |

On failure the script prints `[deploy][warn]` but does not abort — review the
log for warnings and re-run with `--force` after fixing the cause.

### 4.2 Manual verification (operator checklist)

Run these after `scripts/deploy.sh` completes (or after any `stellar contract
invoke`):

```sh
# Source the persisted env
source .soroban-amm.deploy.env

# 1. Factory live and hashes registered
stellar contract invoke --id $FACTORY_CONTRACT_ID --network $NETWORK --source $SOURCE_ACCOUNT -- get_pool_count
stellar contract invoke --id $FACTORY_CONTRACT_ID -- get_pool --token_a $TOKEN_A_CONTRACT_ID --token_b $TOKEN_B_CONTRACT_ID

# 2. Pool info matches expectations
stellar contract invoke --id $AMM_POOL_CONTRACT_ID -- get_info
# Check: token_a == TOKEN_A_CONTRACT_ID, token_b == TOKEN_B_CONTRACT_ID,
#        fee_bps == 30, protocol_fee_bps == 0, admin is governance or ADMIN_ADDRESS

# 3. LP token admin is the pool
stellar contract invoke --id $LP_TOKEN_CONTRACT_ID -- admin
# Must equal $AMM_POOL_CONTRACT_ID

# 4. CL pool at expected tick
stellar contract invoke --id $CL_POOL_CONTRACT_ID -- current_tick
# Must be 0 (or whatever initial_tick you passed)

# 5. Governance points at correct pool/LP
stellar contract invoke --id $GOVERNANCE_CONTRACT_ID -- get_params

# 6. LP token locker is governance
stellar contract invoke --id $LP_TOKEN_CONTRACT_ID -- locker
# Must equal $GOVERNANCE_CONTRACT_ID

# 7. Oracle aggregator admin
stellar contract invoke --id $ORACLE_AGGREGATOR_CONTRACT_ID -- get_admin

# 8. TWAP consumer keeper
stellar contract invoke --id $TWAP_CONSUMER_CONTRACT_ID -- get_keeper

# 9. Staking pool info
stellar contract invoke --id $STAKING_CONTRACT_ID -- get_pool_info

# 10. Full address dump
cat .soroban-amm.deploy.env
```

If any read returns `AlreadyInitialized`, `NotInitialized`, or an empty value,
re-run the relevant step with `--only <contract> --force` or invoke the
`initialize` manually.

### 4.3 End-to-end smoke test

The repo ships `scripts/e2e.sh` — it deploys fresh contracts (or reuses the
deploy helpers), mints tokens, adds liquidity, swaps, and removes liquidity,
asserting reserves and swap output are within expected bounds:

```sh
bash scripts/e2e.sh
# Exits non-zero on any failed assertion; prints [PASS]/[FAIL] summary
```

The flows live in `scripts/e2e/`. The shared flows (`v2`, `factory`, `cl`,
`governance`, `staking`) run against the addresses `deploy.sh` persists. The
self-contained flows (`token`, `router`, `dex_aggregator`,
`oracle_aggregator`, `cl_position_nft`, `pol_vesting`, `twap_consumer`)
deploy their own instances and can be run one at a time without a full
deployment, e.g. `bash scripts/e2e/router.sh`. Select flows with
`--only`/`--skip` (see `bash scripts/e2e/run.sh --help`); the self-contained
flows fund extra signers through friendbot, so they target testnet.

Run this against the same `NETWORK` and `SOURCE_ACCOUNT` after a deployment
to confirm the core path is healthy. For a production deployment, consider a
testnet run before targeting mainnet.

---

## 5. Upgrade Procedure

Contracts are upgradeable via `upgrade(new_wasm_hash)` (or factory's
`update_wasm_hashes` for future pools). Instance storage is preserved; only
bytecode is replaced. **Storage layout is immutable** — changing `DataKey`
variants or types without a migration bricks the contract.

### 5.1 Prerequisites

1. The new WASM must already be uploaded: `stellar contract upload --wasm
   target/wasm32v1-none/release/<contract>.wasm --network $NETWORK --source
   $SOURCE_ACCOUNT` → note the printed `hash`.
2. The caller must be the stored `admin` (or governance, if the pool is
   governance-controlled) — `require_auth()` is enforced.
3. Verify the new binary locally: `cargo build --release --target
   wasm32v1-none`, `stellar contract optimize --wasm ...`, and `cargo test
   --workspace`.

### 5.2 Upgrade steps (per contract)

```sh
source .soroban-amm.deploy.env

# 1. Build and upload new WASM
cargo build --release --target wasm32v1-none
stellar contract upload --wasm target/wasm32v1-none/release/amm.wasm --network $NETWORK --source $SOURCE_ACCOUNT
# -> NEW_HASH=abc123...

# 2. Upgrade the live contract (admin auth required)
stellar contract invoke --id $AMM_POOL_CONTRACT_ID --network $NETWORK --source $ADMIN_ADDRESS -- upgrade --new_wasm_hash $NEW_HASH

# 3. Verify upgrade took — read back liveness and a known getter
stellar contract invoke --id $AMM_POOL_CONTRACT_ID -- get_info
# Or check the WASM hash via RPC if exposed; otherwise publish an `upgraded` event:
stellar contract invoke --id $AMM_POOL_CONTRACT_ID -- get_pool_count  # factory example

# 4. Persist the new hash locally
echo "export AMM_WASM_HASH=$NEW_HASH" >> .soroban-amm.deploy.env
```

For the **factory**, the upgrade path is:

```sh
# Upgrade factory bytecode itself
stellar contract invoke --id $FACTORY_CONTRACT_ID -- upgrade --new_wasm_hash $NEW_HASH
# Update future-pool WASM hashes (existing pools unaffected):
stellar contract invoke --id $FACTORY_CONTRACT_ID -- update_wasm_hashes --amm_wasm_hash $NEW_AMM_HASH --token_wasm_hash $NEW_TOKEN_HASH
```

### 5.3 Rolling back

Roll back by uploading the previous WASM and invoking `upgrade` again with its
hash. Keep the previous hash in `.soroban-amm.deploy.env` history or in git
tags:

```sh
stellar contract invoke --id $FACTORY_CONTRACT_ID -- upgrade --new_wasm_hash $PREVIOUS_HASH
```

There is no automatic rollback — the operator must manually re-invoke `upgrade`
with the old hash. Test upgrades on testnet first and keep a multisig
"time-delayed upgrade" proposal if the contract is governance-controlled (see
§6).

### 5.4 Who authorizes and how to verify

- **Direct admin:** `ADMIN_ADDRESS` key signs `upgrade`. Rotate via
  `propose_admin`/`accept_admin` (see §7).
- **Governance-controlled pools:** `execute` of a `ProposalKind::UpdateFee` or
  `UpdateFactoryGlobalFee` variant triggers the upgrade path — no direct admin
  call.
- **Verification:** After `upgrade`, call a read-only getter (`get_info`,
  `get_params`, `get_pool_count`) and confirm it still returns the expected
  state. Query the RPC for the `upgraded` event topic to audit the hash change.

---

## 6. Emergency Procedures

### 6.1 Pausing a pool

```sh
# Anyone with admin auth can pause — halts swaps, add/remove liquidity, flash loans
stellar contract invoke --id $AMM_POOL_CONTRACT_ID -- pause --admin $ADMIN_ADDRESS
# Check:
stellar contract invoke --id $AMM_POOL_CONTRACT_ID -- is_paused
# -> true

# Resume:
stellar contract invoke --id $AMM_POOL_CONTRACT_ID -- unpause --admin $ADMIN_ADDRESS
```

The CL pool has the same `pause`/`unpause` interface. Factory pools can be
paused individually; there is no global factory pause except `pause_creation`
which blocks `create_pool`/`create_cl_pool`.

**Who can pause:** The pool's stored `admin` (factory admin or governance
contract). Governance-controlled pools require a governance proposal to pause;
direct admin `pause` is blocked when governance is set.

### 6.2 Circuit-breaker auto-pause and recovery

The AMM's `check_circuit_breaker` runs on every `swap`/`add_liquidity`:

- Captures `spot_price = reserve_b * 1_000_000 / reserve_a` at the start of
  each ledger (`CircuitBreakerLastPrice` + `LastSeqno`).
- On the same ledger, if `|current_price - baseline| * 10000 / baseline >=
  threshold_bps`, the pool **auto-pauses** and stores
  `CircuitBreakerTriggeredAt = now`, emitting `circuit_break`.
- Sub-calls revert with `CircuitBreaker (15)`.

**Recovery:**

```sh
# Wait at least cooldown seconds (default 600s == 10 min) after trigger
stellar contract invoke --id $AMM_POOL_CONTRACT_ID -- get_circuit_breaker_config
# -> { threshold_bps: 5000, cooldown_secs: 600, triggered_at: 1714000000, tripped: true }

stellar contract invoke --id $AMM_POOL_CONTRACT_ID -- try_circuit_breaker_recovery
# -> true if cooldown elapsed and pool was tripped; reverts otherwise

# Admin can also unpause directly without waiting (governance review path):
stellar contract invoke --id $AMM_POOL_CONTRACT_ID -- unpause --admin $ADMIN_ADDRESS

# Tune threshold/cooldown (admin-only):
stellar contract invoke --id $AMM_POOL_CONTRACT_ID -- set_circuit_breaker_config --threshold_bps 3000 --cooldown_secs 1800
```

**Who can do each:** `try_circuit_breaker_recovery` is permissionless after
cooldown; `set_circuit_breaker_config` and direct `unpause` require admin.

### 6.3 Multisig emergency withdrawal (k-of-n)

When `set_multisig(signers, quorum)` has been called on the AMM pool, the
single-admin `emergency_withdraw` path is **disabled** — funds can only be
moved via the multisig flow:

```sh
# 1. A signer proposes the recipient
stellar contract invoke --id $AMM_POOL_CONTRACT_ID -- propose_emergency_withdraw --signer $SIGNER_A --recipient $SAFE_ADDRESS

# 2. Other signers approve the same recipient (each calls the same function — approvals accumulate)
stellar contract invoke --id $AMM_POOL_CONTRACT_ID -- propose_emergency_withdraw --signer $SIGNER_B --recipient $SAFE_ADDRESS
# ... until approvals >= quorum

# 3. Any signer executes once quorum is reached and before expiry (7 days)
stellar contract invoke --id $AMM_POOL_CONTRACT_ID -- exec_multisig_emergency_wd --signer $SIGNER_A

# 4. Verify
stellar contract invoke --id $AMM_POOL_CONTRACT_ID -- get_multisig_proposal
# Reserves are zeroed; TotalShares reset; MinLiquidityLocked cleared so deposits can resume
```

**Expiry:** Proposals expire `MULTISIG_PROPOSAL_TTL_SECS = 7 days` after the
`propose` timestamp. Re-proposing with the same signer before expiry does
**not** extend the TTL (fix for indefinite-stalling attack) — only a new
approver extends the window.

**Who can do each:** Only addresses in `signers` Vec; `quorum` of them must
approve. `AlreadyExecuted (19)` / `ProposalExpired (20)` are returned on
double-execute or late execute.

### 6.4 Factory creation pause

```sh
# Pause new pool creation (does not affect existing pools)
stellar contract invoke --id $FACTORY_CONTRACT_ID -- pause_creation --admin $ADMIN_ADDRESS
# Resume
stellar contract invoke --id $FACTORY_CONTRACT_ID -- unpause_creation --admin $ADMIN_ADDRESS
# Check
stellar contract invoke --id $FACTORY_CONTRACT_ID -- is_creation_paused
```

Use during an active incident to prevent attackers from creating pools with
manipulated fees while the root cause is investigated.

---

## 7. Admin Key Management

Every contract uses a **two-step `propose_admin` / `accept_admin`** rotation —
there is no single-transaction admin steal. The pattern is identical across
`token`, `amm`, `factory`, `oracle_aggregator`, `batch_auction`, etc.

### 7.1 Rotation procedure

```sh
# 1. Current admin nominates the new admin
stellar contract invoke --id $CONTRACT_ID -- propose_admin --current_admin $OLD --new_admin $NEW

# 2. New admin must accept from its own key (require_auth on $NEW)
stellar contract invoke --id $CONTRACT_ID -- accept_admin --new_admin $NEW \
  --source $NEW_KEY

# Verify:
stellar contract invoke --id $CONTRACT_ID -- admin   # or get_admin / get_info
```

If step 2 is not completed, the nomination sits as `PendingAdmin` and the old
admin remains in control. Calling `accept_admin` with a wrong address reverts
`WrongAdmin (13)`; calling without a pending nomination reverts
`NoPendingAdmin (12)`.

**Protocol-wide rotation** — repeat for every pool the factory has deployed,
plus each auxiliary contract. Enumerate with `get_pool_count` +
`get_pools(offset, limit)` (and `get_cl_pool_count` + `get_cl_pools` for CL
pools). Do **not** drive the loop off `all_pools()`: it is capped at 200
entries (issue #790) and silently truncates past that, which would leave the
pools beyond the cap still under the old admin. Use `scripts/deploy.sh --only
<contract>` or a script loop:

```sh
source .soroban-amm.deploy.env
page=50
count=$(stellar contract invoke --id $FACTORY_CONTRACT_ID -- get_pool_count)
for ((offset = 0; offset < count; offset += page)); do
  pools=$(stellar contract invoke --id $FACTORY_CONTRACT_ID \
    -- get_pools --offset "$offset" --limit "$page" | grep -Eo 'C[A-Z0-9]{55}')
  for pool in $pools; do
    echo "Rotating $pool"
    stellar contract invoke --id $pool -- propose_admin --current_admin $OLD --new_admin $NEW --source $OLD
    stellar contract invoke --id $pool -- accept_admin --new_admin $NEW --source $NEW
  done
done
```

### 7.2 Multisig configuration

- **AMM multisig:** `set_multisig(signers Vec<Address>, quorum u32)` on the
  pool. Set `quorum=0` to disable multisig (single-admin mode). Changing the
  config clears any pending `MultisigProposal`.
- **Governance veto multisig:** `set_veto_multisig(multisig Address)` on
  governance. Within `VETO_WINDOW_SECS=86400` after a proposal's `vote_end`,
  the veto multisig can `veto(proposal_id)` to mark it `ProposalVetoed (28)`.
- **Factory treasury:** `set_treasury(admin, treasury Address, global_protocol_fee_bps)` — makes the factory the `fee_recipient` for all factory pools, then `sweep_fees(token)` (permissionless) forwards accrued fees to treasury. Rotate treasury via `propose_treasury`/`accept_treasury` if exposed.
- **LP token `set_locker`:** Governance is the locker; if governance is upgraded, rotate locker via `set_locker(new_governance)`. The fix for issue #556 ensures old locker entries remain unlockable by the locker that originally locked them.

**Recommendation:** Use a multisig (e.g. 3-of-5) for all `admin` roles on
mainnet. Single-key admins are appropriate only on testnet.

---

## 8. Network-Specific Notes

### Testnet

- Friendbot funds any new keypair; `stellar keys fund --network testnet` works.
- RPC `https://soroban-testnet.stellar.org` may rate-limit under load — retry
  with exponential backoff; `scripts/deploy.sh` does not retry automatically.
- Storage rent is lower; deploys are cheap — use testnet for all rehearsal
  runs before mainnet.
- Run `scripts/e2e.sh` on testnet after `scripts/deploy.sh` to smoke-test
  swaps, liquidity, and factory discovery.
- WASM uploads and deploys are fast; the `--fund` flag on `keys generate` is
  sufficient.

### Mainnet

- No Friendbot — the deployer must be funded before running the script.
  Transfer at least 20–30 XLM for a full protocol deployment to cover transaction
  fees and contract instance storage rent.
- Use `--network mainnet` and `--source mainnet-deployer` (a dedicated,
  hardware-wallet-backed key if possible).
- Double-check all recommended parameters before mainnet — especially `fee_bps`,
  `protocol_fee_bps`, `voting_period`, `quorum_bps`, and `max_staleness`.
  Parameter regressions on mainnet require governance proposals or coordinated
  `upgrade` calls.
- The Stellar CLI network passphrase for mainnet is `Public Global Stellar
  Network ; September 2015` — verify `stellar network add mainnet` uses the
  correct RPC and passphrase.
- Consider `make optimize` before upload — optimized binaries reduce storage
  rent (20–40% smaller).
- Deploy on mainnet with `--only` increments and verify each increment before
  proceeding, rather than a single full-protocol run — this limits blast radius
  of a failed batch.
- Keep `ADMIN_ADDRESS` as a multisig contract address from day one on mainnet;
  single-key admin on mainnet is an existential risk.

### Local / standalone

- Use `stellar network add local --rpc-url http://localhost:8000 ...` with
  `stellar network` or `soroban` quickstart.
- Fund the deployer via the local Friendbot or `stellar keys fund`.

---

## 9. Failure Modes & Error Codes

Cross-reference `docs/error-codes.md` for the full discriminant table. Common
operator-facing failures and their fixes:

### AMM (`AmmError`)

| Code | Variant | Operator action |
|------|---------|-----------------|
| 1 | AlreadyInitialized | `initialize` called twice — deploy is idempotent; re-run verification instead. |
| 2 | InvalidFeeBps | `fee_bps` out of `[0,10000]` or `protocol_fee_bps >= fee_bps`. Fix param and redeploy or `update_fee`. |
| 4 | DeadlineExceeded | `deadline < ledger.timestamp`. Increase deadline (caller should set `now + 300`). |
| 5 | SlippageExceeded | `amount_out < min_out` or `amount_in > max_in`. Widen slippage or pre-quote with `get_amount_out`. |
| 6 | Paused | Pool is paused (admin or circuit breaker). Check `is_paused` / `get_circuit_breaker_config`. |
| 11 | InsufficientLiquidity | Swap or remove larger than reserves. Fund pool or reduce amount. |
| 14 | Reentrant | Receiver callback re-entered the pool — fix receiver contract, not the pool. |
| 15 | CircuitBreaker | Price moved >threshold in one ledger. See §6.2 recovery. |
| 18 | FlashLoanRepaymentFailed | Receiver did not return `amount + fee` — fix `on_flash_loan` implementation. |
| 19/20 | AlreadyExecuted / ProposalExpired | Multisig proposal lifecycle error — re-propose. |

### Factory (`FactoryError`)

| Code | Variant | Operator action |
|------|---------|-----------------|
| 1 | AlreadyInitialized | Factory already `initialize`d — skip or verify `get_pool_count`. |
| 3 | PoolAlreadyExists | Pair already has a pool — query `get_pool` instead of creating. |
| 4 | ClPoolAlreadyExists | Same triplet exists — query `get_cl_pool`. |
| 5 | ClWasmNotSet | Call `set_cl_wasm_hash` before `create_cl_pool`. |
| 6 | Unauthorized | Not the factory `admin` (or not permissionless caller). Use correct `admin` key. |
| 8 | RateLimitExceeded | `LastPoolCreation` within `RateLimitLedgers`. Wait or lower rate limit via `set_rate_limit`. |
| 9 | CreationPaused | `pause_creation` is active — `unpause_creation`. |

### Governance (`GovernanceError`)

| Code | Variant | Operator action |
|------|---------|-----------------|
| 8 | InsufficientStake | Proposer holds < `min_proposer_stake_bps` of LP supply. Increase holdings or lower threshold via `set_min_proposer_stake_bps`. |
| 9 | ProposalNotFound | `proposal_id` does not exist — check `proposal_status`. |
| 11 | VotingPeriodEnded | Vote window closed — new proposal required. |
| 19 | QuorumNotMet | `for_votes / total_supply < quorum_bps/10000`. Wait for more voters or lower quorum. |
| 28 | ProposalVetoed | Veto multisig rejected — cannot be re-executed. |
| 34 | PartialFactoryUpdate | `UpdateFactoryGlobalFee` window (`offset/limit`) did not cover all pools — re-propose with full window. |

### Token (panic messages, not enumerants)

| Message | Operator action |
|---------|-----------------|
| `already initialized: contract ...` | `initialize` called twice — skip. |
| `insufficient allowance: ...` | `approve` with higher `amount` / later `live_until_ledger`. |
| `live_until_ledger must be >= current ledger` | Approval expired — re-approve with `live_until_ledger >= ledger.sequence`. |
| `insufficient unlocked balance: ...` | Tokens locked by governance vote — call `unlock` after proposal concludes. |
| `current_admin is not admin` | Wrong `current_admin` in `propose_admin`. |
| `not pending admin` | Wrong `new_admin` in `accept_admin`. |

### CL (`ClError`)

| Code | Meaning | Operator action |
|------|---------|-----------------|
| 4 | TickOutOfRange | `tick ∉ [-887272, 887272]` |
| 13 | TickNotAligned | Tick not multiple of `tick_spacing` — adjust tick bounds. |
| 16 | InvalidToken | `token_in` not in pool's pair. |
| 18 | OracleDeviationExceeded | Swap vs. oracle price > `MaxOracleDeviationBps` — check aggregator sources. |
| 19 | NftNotConfigured | `set_position_nft` not called — wire NFT contract. |

For the full discriminant table see `docs/error-codes.md`.

---

## 10. Deployment Script Reference

### Quick start

```sh
# Full protocol to testnet (resumable, idempotent, verified)
scripts/deploy.sh testnet

# Only the core path (factory + pools + governance)
scripts/deploy.sh --only factory,pools,governance

# Incremental: deploy one contract
scripts/deploy.sh --only staking

# Redeploy after a failed run (without --force the completed steps are no-ops)
scripts/deploy.sh testnet   # resumes
scripts/deploy.sh testnet --force  # re-does everything

# Skip governance-dependent contracts
scripts/deploy.sh --skip governance,staking,incentive_campaigns

# Mainnet (explicit source and network)
NETWORK=mainnet SOURCE_ACCOUNT=mainnet-deployer scripts/deploy.sh --force
```

### Env file contract

All state is persisted to `.soroban-amm.deploy.env` (overridable via
`$DEPLOY_ENV`) as `export KEY='value'` lines — **every address and initialization
marker is written immediately after it succeeds**, so:

- **Killing the script mid-deployment and re-running resumes** rather than
  restarting. Demonstrate:

  ```sh
  scripts/deploy.sh testnet & pid=$!
  sleep 5; kill $pid
  cat .soroban-amm.deploy.env   # partial addresses persisted
  scripts/deploy.sh testnet      # resumes from where it left off
  ```

- **Re-running a completed deployment is a no-op** without `--force`:

  ```sh
  scripts/deploy.sh testnet           # second run: all steps print "skipping ... already at ..."
  scripts/deploy.sh testnet --force   # re-deploys and re-initializes
  ```

- **Every deployed address is persisted as it is created**, not at the end.

- **A failure prints which contract and which step failed**:

  ```
  [deploy][error] failed at contract=factory step=initialize factory (exit 1)
  ```

### Reusing helpers in other scripts (e.g. `scripts/e2e.sh`)

All `scripts/deploy/*.sh` modules are **sourceable**. To extend `scripts/e2e.sh`
or write a custom integration script:

```sh
ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
NETWORK=testnet
DEPLOY_ENV="$ROOT_DIR/.soroban-amm.e2e.env"
source "$ROOT_DIR/scripts/deploy/common.sh"
source "$ROOT_DIR/scripts/deploy/token.sh"
# Now call deploy_tokens, invoke, persist_var, etc.
```

Keep deploy helpers importable — do not inline their logic into `main`
(see `Coordination` in the deploy issue).

### Out of scope

The deploy script does **not** modify `scripts/e2e.sh`, CI workflows, or
contract source — those are owned by other issues. Contract size reports and
optimizer scripts (`scripts/size_report.sh`, `scripts/optimize_contracts.sh`)
are also untouched.

---

## Appendix: Full initializer index

| Contract | Function | File |
|----------|----------|------|
| token | `initialize(admin, name, symbol, decimals)` | `contracts/token/src/lib.rs` |
| amm | `initialize(admin, token_a, token_b, lp_token, fee_bps, fee_recipient, protocol_fee_bps)` | `contracts/amm/src/lib.rs` |
| factory | `initialize(admin, amm_wasm_hash, token_wasm_hash)` + `set_cl_wasm_hash(cl_wasm_hash)` | `contracts/factory/src/lib.rs` |
| factory | `create_pool(caller, token_a, token_b, fee_tier, governance_wasm_hash)` |  |
| factory | `create_cl_pool(caller, token_a, token_b, fee_bps, initial_tick)` |  |
| governance | `initialize(admin, amm_pool, lp_token, voting_period_secs, timelock_secs, quorum_bps, min_proposer_stake_bps)` | `contracts/governance/src/lib.rs` |
| staking | `initialize(lp_token, reward_token, admin)` | `contracts/staking/src/lib.rs` |
| concentrated_liquidity | `initialize(admin, token_a, token_b, fee_bps, initial_tick, tick_spacing)` | `contracts/concentrated_liquidity/src/lib.rs` |
| twap_consumer | `initialize(keeper)` | `contracts/twap_consumer/src/lib.rs` |
| twal_consumer | `initialize(keeper)` | `contracts/twal_consumer/src/lib.rs` |
| oracle_aggregator | `initialize(admin, max_staleness_seconds)` | `contracts/oracle_aggregator/src/lib.rs` |
| router | `initialize(factory)` | `contracts/router/src/lib.rs` |
| batch_router | `initialize(factory)` | `contracts/batch_router/src/lib.rs` |
| dex_aggregator | `initialize(admin, factory)` | `contracts/dex_aggregator/src/lib.rs` |
| batch_auction | `initialize(admin, batch_window_secs)` | `contracts/batch_auction/src/lib.rs` |
| cl_position_nft | `initialize(admin, cl_pool)` | `contracts/cl_position_nft/src/lib.rs` |
| reserve_manager | `initialize(governance, factory)` | `contracts/reserve_manager/src/lib.rs` |
| incentive_campaigns | `initialize(governance)` | `contracts/incentive_campaigns/src/lib.rs` |
| pol_vesting | `initialize(governance, treasury)` | `contracts/pol_vesting/src/lib.rs` |
| v2_to_v3_migration | `initialize(admin, v2_pool, v3_pool)` | `contracts/v2_to_v3_migration/src/lib.rs` |

---

*This runbook is the operational companion to `README.md` and
`docs/error-codes.md`. Keep it up to date with every initializer, fee, or
governance change in a PR that touches `scripts/deploy/**`.*
