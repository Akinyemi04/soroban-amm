# AMM Pool Management Best Practices

A comprehensive guide for deploying, configuring, and operating AMM pools on Soroban.

---

## 1. Fee Tier Selection

Choosing the right fee tier is the single most impactful configuration decision. The pool fee (`fee_bps`) is set at initialization and can only be changed later via governance.

| Fee (bps) | Fee (%) | Best for |
|---|---|---|
| 1 | 0.01% | Pegged pairs (e.g. USDC/USDT) — minimal slippage, high volume |
| 5 | 0.05% | Highly correlated assets (e.g. stablecoin variants) |
| 30 | 0.30% | Standard volatile pairs (e.g. XLM/USDC) — the most common choice |
| 100 | 1.00% | Exotic or low-liquidity pairs with high price volatility |

**Rules of thumb:**
- Start with 30 bps for any new volatile pair. It matches the market expectation and attracts the most integrators.
- Use 5 bps only when both tokens track the same underlying (e.g. two USD stablecoins). Higher fees will drive volume to competitors.
- Use 100 bps when the pair has thin liquidity and large price swings. The higher fee compensates LPs for impermanent loss.
- `protocol_fee_bps` must always be ≤ `fee_bps`. A common split is `fee_bps = 30`, `protocol_fee_bps = 5` (5 bps to the protocol, 25 bps to LPs).

**Changing fees post-deployment:**

Fees can be updated via `update_fee` (admin-only) or through a governance `UpdateFee` proposal. Always simulate the impact before executing:

```sh
# Read current fee
stellar contract invoke --id <POOL> -- get_info

# Propose a fee change via governance
stellar contract invoke --id <GOVERNANCE> --source <PROPOSER> \
  -- propose \
  --proposer <PROPOSER_ADDRESS> \
  --kind '{"UpdateFee": 20}'
```

---

## 2. Pool Initialization Checklist

Before calling `initialize`, verify:

1. Both token contracts are deployed and SEP-41 compliant.
2. The LP token contract is deployed with the AMM pool address as its admin.
3. `token_a != token_b`.
4. `fee_bps` is in `[0, 10_000]`.
5. `protocol_fee_bps` is in `[0, fee_bps]`.
6. The `fee_recipient` address is a multisig or a contract — not a single hot wallet.
7. The `admin` address is a multisig or a governance contract for production deployments.

```sh
# Initialize via factory (recommended)
stellar contract invoke --id <FACTORY> --source <ADMIN> \
  -- create_pool \
  --token_a <TOKEN_A> \
  --token_b <TOKEN_B> \
  --fee_bps 30

# Or manually
stellar contract invoke --id <POOL> --source <ADMIN> \
  -- initialize \
  --admin <ADMIN_ADDRESS> \
  --token_a <TOKEN_A> \
  --token_b <TOKEN_B> \
  --lp_token <LP_TOKEN> \
  --fee_bps 30 \
  --fee_recipient <FEE_RECIPIENT> \
  --protocol_fee_bps 5
```

---

## 3. Liquidity Provisioning Strategies

### 3.1 Initial Seeding

The first deposit sets the pool price. Deposit tokens in the ratio that reflects the current market price. If XLM trades at 0.10 USDC, seed with 10 XLM per 1 USDC.

```
Initial shares = sqrt(amount_a * amount_b)
```

A seed of 1,000,000 XLM and 100,000 USDC yields `sqrt(1e6 * 1e5) ≈ 316,227` LP shares.

Set `min_shares = 0` only for the initial deposit. For all subsequent deposits, compute the expected shares off-chain and set a tight `min_shares`.

### 3.2 Subsequent Deposits

The pool accepts any ratio but mints shares based on the *lesser* of the two proportional contributions:

```
shares = min(
    amount_a * total_shares / reserve_a,
    amount_b * total_shares / reserve_b
)
```

Excess tokens beyond the pool ratio are accepted but do not earn additional shares. Always compute the optimal ratio before depositing:

```typescript
// Compute optimal amount_b given amount_a
const optimalB = (amountA * reserveB) / reserveA;
```

### 3.3 Slippage Protection

Always set non-zero `min_shares` for production deposits. A 0.5–1% tolerance is typical:

```sh
stellar contract invoke --id <POOL> --source <PROVIDER> \
  -- add_liquidity \
  --provider <PROVIDER_ADDRESS> \
  --amount_a 1000000 \
  --amount_b 1000000 \
  --min_shares 990000 \
  --deadline <NOW_PLUS_60_SECONDS>
```

### 3.4 Removing Liquidity

Use `remove_liquidity` for proportional withdrawal. Use `remove_liquidity_one_sided` when you want to exit into a single token — it performs an internal swap and saves a separate transaction.

```sh
# Proportional removal
stellar contract invoke --id <POOL> --source <PROVIDER> \
  -- remove_liquidity \
  --provider <PROVIDER_ADDRESS> \
  --shares <LP_AMOUNT> \
  --min_a 0 \
  --min_b 0 \
  --deadline <DEADLINE>

# Single-token exit (receives only token_a)
stellar contract invoke --id <POOL> --source <PROVIDER> \
  -- remove_liquidity_one_sided \
  --provider <PROVIDER_ADDRESS> \
  --shares <LP_AMOUNT> \
  --token_out <TOKEN_A> \
  --min_out <MIN_AMOUNT> \
  --deadline <DEADLINE>
```

---

## 4. Risk Management for LPs

Providing liquidity to an AMM pool exposes LPs to various risks, including impermanent loss, smart contract risk, and governance risk. Understanding and mitigating these risks is crucial for long-term profitability.

### 4.1 Impermanent Loss

Impermanent loss occurs when the price of the deposited tokens changes relative to when they were deposited. The loss is "impermanent" because if the price returns to the original level, the loss is recovered. However, if the LP withdraws at a different price, the loss becomes permanent.

The impermanent loss can be estimated using the formula:

```
IL = 2 * sqrt(price_ratio) / (1 + price_ratio) - 1
```

where `price_ratio` is the ratio of the current price to the price at deposit.

For example, if the price of token A doubles relative to token B (price_ratio = 2), the impermanent loss is approximately 5.7%.

To mitigate impermanent loss:
- Choose pairs with low volatility (e.g., stablecoin pairs).
- Consider providing liquidity to pools with correlated assets.
- Monitor the price ratio and withdraw if the impermanent loss exceeds your tolerance.

### 4.2 Smart Contract Risk

While the AMM contracts have been audited, there is always a risk of vulnerabilities in smart contracts. To mitigate this risk:
- Only interact with contracts that have been audited by reputable firms.
- Check the contract's upgradeability features and ensure that upgrades require governance approval.
- Consider using a multisig wallet for large LP positions to require multiple approvals for transactions.

### 4.3 Governance Risk

LP token holders have governance rights over the pool, including the ability to change fees and upgrade the contract. However, this also means that malicious proposals could harm LPs. To mitigate governance risk:
- Participate in governance votes to protect your interests.
- Delegating your voting power to a trusted entity if you cannot participate actively.
- Monitor governance proposals and vote against those that could negatively impact the pool.

### 4.4 Mitigation Strategies

Overall, LPs can adopt the following strategies to manage risk:
- Diversify across multiple pools and asset pairs.
- Start with small positions to test the pool's performance.
- Regularly monitor pool metrics such as reserves, volume, and fee accruals.
- Consider using insurance funds or hedging strategies if available.

---

## 5. Admin Key Management

The pool admin can pause the pool, update fees, set the protocol fee, and initiate contract upgrades. Compromise of the admin key is a critical security event.

**Best practices:**

- Use a 2-of-3 multisig for the admin address on mainnet.
- Transfer admin to a governance contract once the pool is established.
- Use the two-step `propose_admin` / `accept_admin` pattern for all admin transfers — never transfer directly.

```sh
# Step 1: current admin nominates a new admin
stellar contract invoke --id <POOL> --source <CURRENT_ADMIN> \
  -- propose_admin \
  --current_admin <CURRENT_ADMIN_ADDRESS> \
  --new_admin <NEW_ADMIN_ADDRESS>

# Step 2: new admin accepts
stellar contract invoke --id <POOL> --source <NEW_ADMIN> \
  -- accept_admin \
  --new_admin <NEW_ADMIN_ADDRESS>
```

---

## 6. Governance Participation

LP token holders govern the pool through the governance contract. Participation protects your position from unilateral fee changes or parameter updates.

### 6.1 Proposing a Change

You need at least `min_proposer_stake_bps / 10_000 * total_supply` LP tokens to propose.

```sh
# Check your stake and the minimum required
stellar contract invoke --id <GOVERNANCE> -- get_params
stellar contract invoke --id <POOL> -- shares_of --provider <YOUR_ADDRESS>

# Propose a fee reduction from 30 to 20 bps
stellar contract invoke --id <GOVERNANCE> --source <PROPOSER> \
  -- propose \
  --proposer <PROPOSER_ADDRESS> \
  --kind '{"UpdateFee": 20}'
```

### 6.2 Voting

Voting power equals your LP token balance at the time you vote. Tokens are locked until the proposal resolves.

```sh
# Vote in favour
stellar contract invoke --id <GOVERNANCE> --source <VOTER> \
  -- vote \
  --voter <VOTER_ADDRESS> \
  --proposal_id 1 \
  --support true

# Check proposal status
stellar contract invoke --id <GOVERNANCE> -- proposal_status --proposal_id 1
```

### 6.3 Executing and Unlocking

After the voting period ends and the timelock elapses, anyone can execute a passing proposal.

```sh
# Execute
stellar contract invoke --id <GOVERNANCE> -- execute --proposal_id 1

# Unlock your LP tokens
stellar contract invoke --id <GOVERNANCE> --source <VOTER> \
  -- unlock_vote \
  --voter <VOTER_ADDRESS> \
  --proposal_id 1
```

---

## 7. Circuit Breaker

The pool has a built-in circuit breaker that auto-pauses when the spot price deviates more than `threshold_bps` (default 5,000 = 50%) within a single ledger sequence. This protects against flash-loan price manipulation.

```sh
# Read current circuit breaker config
stellar contract invoke --id <POOL> -- get_circuit_breaker_config

# Attempt automatic recovery after cooldown (default 600s)
stellar contract invoke --id <POOL> -- try_circuit_breaker_recovery

# Admin can also manually unpause
stellar contract invoke --id <POOL> --source <ADMIN> -- unpause
```

To tune the circuit breaker for a more volatile pair:

```sh
stellar contract invoke --id <POOL> --source <ADMIN> \
  -- set_circuit_breaker_config \
  --threshold_bps 8000 \
  --cooldown_secs 300
```

---

## 8. Protocol Fee Management

Protocol fees accrue in the pool and must be explicitly withdrawn by the `fee_recipient`.

```sh
# Check pending fees without moving funds
stellar contract invoke --id <POOL> -- get_accrued_fees

# Withdraw to fee_recipient
stellar contract invoke --id <POOL> --source <FEE_RECIPIENT> \
  -- withdraw_protocol_fees
```

Set up a periodic keeper job (e.g. a cron script calling the Stellar RPC) to withdraw fees regularly. Fees left in the pool do not compound — they sit idle until withdrawn.

---

## 9. Monitoring and Alerting

Key metrics to monitor for a healthy pool:

| Metric | How to read | Alert threshold |
|---|---|---|
| Reserve ratio | `get_info().reserve_a / reserve_b` | > 10× drift from initial ratio |
| Total shares | `get_info().total_shares` | Sudden drop > 20% in one block |
| Accrued fees | `get_accrued_fees()` | Exceeds a configured USD threshold |
| Paused state | `is_paused()` | Any `true` value |
| Circuit breaker | `get_circuit_breaker_config().tripped` | Any `true` value |
| TWAP vs spot | `price_ratio()` vs TWAP consumer | Deviation > 5% |

Subscribe to on-chain events for real-time alerts:

| Event | Meaning |
|---|---|
| `circuit_break` | Pool auto-paused; investigate immediately |
| `emergency_withdraw` | Admin drained reserves; critical incident |
| `admin_nominated` | Admin transfer initiated; verify it is expected |
| `admin_changed` | Admin transfer completed |

---

## 10. Upgrade Procedure

Contract upgrades replace bytecode while preserving all storage. Follow this checklist:

1. Upload the new WASM: `stellar contract upload --wasm new_amm.wasm`
2. Verify the hash matches the audited binary.
3. Test on testnet with the same storage state.
4. Create a governance proposal (`TransferAdmin` or direct `upgrade` call).
5. After the timelock elapses, execute the upgrade.
6. Verify `get_info()` returns expected values post-upgrade.

```sh
stellar contract invoke --id <POOL> --source <ADMIN> \
  -- upgrade \
  --new_wasm_hash <NEW_WASM_HASH>
```

Storage keys (`DataKey` variants) must not be renamed or reordered across upgrades — doing so constitutes a breaking change requiring a migration.