# Security Audit — Soroban AMM Pool Contract

**Scope:** `contracts/amm/src/lib.rs` (constant-product pool, reviewed). `contracts/concentrated_liquidity/src/lib.rs` (concentrated-liquidity pool) is listed as outstanding review scope in [§3](#3-outstanding-review-scope-concentrated-liquidity).
**Audit type:** Internal property-based audit with manual review
**Methodology:** Static analysis, manual code review, and property-based testing via proptest

> **Revision:** 2026-09-27. Describes the source at commit `74d227f` (`main`).
> Finding statuses and line numbers below were re-checked against that commit.
> Line numbers will drift. If one no longer matches, search for the function name.

### Revision history

| Date | Commit | Change |
|------|--------|--------|
| 2026-09-27 | `74d227f` | Re-verified every finding against the current source. Moved the fixed findings to [§2.6 Resolved findings](#26-resolved-findings), with the fixing change and current location for each. Added M-03 (the residual gap from M-02 in `initialize`). Listed the unreviewed concentrated-liquidity surfaces in §3. |
| — | `66a4643` | Original constant-product review. |

---

## 1. Scope

| Component | File | Focus | Review status |
|-----------|------|-------|---------------|
| Constant-product AMM | `contracts/amm/src/lib.rs` | Liquidity accounting, fee calculations, swap invariants | Reviewed |
| Concentrated-liquidity AMM | `contracts/concentrated_liquidity/src/lib.rs` | Tick bounds, positions, liquidity accounting, swaps | **Not reviewed**. See §3 |
| LP token | `contracts/token/src/lib.rs` | Mint/burn authorization | Reviewed |
| Factory | `contracts/factory/src/lib.rs` | Deployment and initialization | Reviewed |
| Fuzz suite | `contracts/amm-fuzz/src/lib.rs` | Property-based invariant verification | Reviewed |

> Scope note: the findings and property results below apply to the
> constant-product pool only. The x*y=k findings must not be applied to the
> concentrated-liquidity contract, which uses tick-based ranges and position
> liquidity. The work still needed to review that contract is listed in §3.

---

## 2. Findings

### Summary

| ID | Title | Severity | Status |
|----|-------|----------|--------|
| M-03 | `initialize` accepts `protocol_fee_bps == fee_bps` | Medium | **Open** |
| L-02 | Imbalanced deposits accept excess tokens without refund | Low | Acknowledged (by design) |
| I-02 | No distinct event for the first deposit | Informational | Open (informational) |
| H-01 | `initialize` ignores `admin`, `fee_recipient`, `protocol_fee_bps` | High | Fixed. See §2.6 |
| M-01 | No re-entrancy guard on `flash_loan` | Medium | Fixed. See §2.6 |
| M-02 | `set_protocol_fee` allows `protocol_fee_bps == fee_bps` | Medium | Fixed. See §2.6 (residual gap: M-03) |
| L-01 | `withdraw_protocol_fees` missing closing brace | Low | Fixed. See §2.6 |
| L-03 | `pause`/`unpause` do not check the stored admin | Low | Fixed. See §2.6 |
| I-01 | TWAP accumulators can overflow | Informational | Addressed. See §2.6 |

### 2.1 Critical

No critical issues identified.

---

### 2.2 High

No open high-severity findings. H-01 is resolved (see §2.6).

---

### 2.3 Medium

#### M-03 — `initialize` accepts `protocol_fee_bps == fee_bps`

**Location:** `contracts/amm/src/lib.rs:381`, in `initialize_with_flash_loan_fee` (`initialize` delegates to it)
**Description:** The M-02 fix made `set_protocol_fee` reject a value of
`protocol_fee_bps` that is equal to or greater than `fee_bps`. The initializer
still validates with `(0..=fee_bps).contains(&protocol_fee_bps)`, which allows
equality. A pool created with `protocol_fee_bps == fee_bps > 0` sends 100% of
swap fees to the protocol and none to LPs, which is the situation M-02 was
meant to prevent. The admin can only move away from that setting later through
`set_protocol_fee`.
**Recommendation:** Apply the same strict bound as `set_protocol_fee` in the
initializer: `protocol_fee_bps < fee_bps` when `fee_bps > 0`, and
`protocol_fee_bps == 0` when `fee_bps == 0`.
**Status:** Open.

---

### 2.4 Low

#### L-02 — Imbalanced deposits accept excess tokens without refund

**Location:** `contracts/amm/src/lib.rs:1552` — `add_liquidity` (doc comment at line 1533)
**Description:** When a deposit is imbalanced, shares are minted at the
minimum ratio and the excess tokens remain in the pool, effectively donating
them to existing LPs. The function does not refund excess.
**Recommendation:** Document this behavior prominently in the function docs
and recommend callers compute optimal amounts off-chain before depositing.
**Status:** Acknowledged (by design, documented). The `add_liquidity` doc
comment says excess tokens are "**not** refunded automatically" and tells
callers to compute amounts off-chain.

---

### 2.5 Informational

#### I-02 — No event emitted by `add_liquidity` for first deposit

**Location:** `contracts/amm/src/lib.rs:1552` — `add_liquidity`
**Description:** The `add_liquidity` event is published for all deposits, but
the first deposit (which sets the pool price) has no distinct event that indexers
can use to detect initial price discovery. As of `74d227f` there is still no
`init_price` (or equivalent) event. The first-deposit branch at line 1600 only
locks `MINIMUM_LIQUIDITY`.
**Recommendation:** Emit a separate `init_price` event on the first deposit.
**Status:** Open (informational).

---

### 2.6 Resolved findings

These findings are kept for their history. Each entry gives the original
finding, the change that fixed it, and where the fix is in the source at
`74d227f`, so a reader can check the fix directly. The PR numbers are the first
upstream merge that contained the fixing commit.

#### H-01 — `initialize` ignores `admin`, `fee_recipient`, and `protocol_fee_bps` — **Fixed**

**Original finding:** `initialize` delegated to `initialize_with_flash_loan_fee`
and passed only `token_a`, `token_b`, `lp_token` and `fee_bps`. The values of
`admin`, `fee_recipient` and `protocol_fee_bps` were silently dropped.
**Fix:** `94fbf5d` ("fix AMM contract compilation errors", merged via #85).
`initialize` now forwards all eight arguments.
**Current location:** `contracts/amm/src/lib.rs:336` (`initialize`) forwards
to `initialize_with_flash_loan_fee` at line 361. That function stores `Admin`,
`FeeRecipient` and `ProtocolFeeBps`.
**Residual:** the initializer's protocol-fee bound is not as strict as the one
in `set_protocol_fee`. This is tracked as M-03.

#### M-01 — No re-entrancy guard on `flash_loan` — **Fixed**

**Original finding:** a flash-loan callback could re-enter `swap` or
`add_liquidity` while reserves were in an inconsistent state.
**Fix:** `525d0d6` added reentrancy guards (merged via #285). `c80c12d` then
extended the guard to every fund-moving entry point (merged via #778).
**Current location:** the RAII `ReentrancyGuard` over `DataKey::Locked` is at
`contracts/amm/src/lib.rs:263–303`. Guards are acquired in `flash_loan` (line
2360), `add_liquidity` (1571), `remove_liquidity` (1704),
`remove_liquidity_one_sided` (1806), `swap` (2022), `swap_exact_out` (2184),
`withdraw_protocol_fees` (2306) and `emergency_withdraw` (518). A reentrant call
returns `AmmError::Reentrant` (line 82). The lock state can be read through
`is_locked` / `flash_loan_locked` (lines 601, 608).

#### M-02 — `set_protocol_fee` allows admin to set `protocol_fee_bps == fee_bps` — **Fixed**

**Original finding:** `protocol_fee_bps <= fee_bps` let the admin send the
whole swap fee to the protocol, leaving nothing for LPs.
**Fix:** `9783353` (merged via #454). The bound is now strict, and a
`protocol_fee_set` event is emitted on every change so LPs and indexers can
monitor it.
**Current location:** `contracts/amm/src/lib.rs:850` — `set_protocol_fee` (the
check follows the `// Fix M-02` comment).
**Residual:** `initialize` still allows equality. This is tracked as M-03.

#### L-01 — `withdraw_protocol_fees` missing closing brace causes dead code — **Fixed**

**Original finding:** at `66a4643`, `withdraw_protocol_fees` ended at the
`(fee_a, fee_b)` return with no closing `}`, so `flash_loan` appeared nested
inside it.
**Fix:** `94fbf5d` (merged via #85) added the missing brace.
**Current location:** `contracts/amm/src/lib.rs:2303` — `withdraw_protocol_fees`
is well-formed and closes after `Ok((fee_a, fee_b))`. `flash_loan` is a
separate method at line 2360. The function has also since gained the
reentrancy guard (M-01) and a typed `NotInitialized` error.

#### L-03 — `pause` and `unpause` do not validate the caller against stored admin — **Fixed**

**Original finding:** `pause(env, admin)` / `unpause(env, admin)` called
`require_auth` on the address passed in and never checked the stored admin.
**Fix:** `d0c0688` ("enforce admin auth", merged via #113). The `admin`
parameter was removed, and both functions now load the stored admin and require
its authorization.
**Current location:** `contracts/amm/src/lib.rs:493` (`pause`) and `:501`
(`unpause`). Both call `Self::read_admin(&env)?` and then `admin.require_auth()`.

#### I-01 — TWAP accumulators can overflow for long-lived pools — **Addressed**

**Original finding:** the `i128` price accumulators could in theory overflow.
The overflow horizon was about 1.6 × 10¹⁷ years, so this was not an immediate
concern.
**Fix:** `f2f7f4d` (merged via #170) switched to `wrapping_add`, which follows
the recommendation. Overflow is now defined behavior, and consumers take
differences of cumulative values.
**Current location:** `contracts/amm/src/lib.rs:1478–1479` in `checkpoint_twap`
(line 1454).

---

## 3. Outstanding review scope: concentrated liquidity

`contracts/concentrated_liquidity/src/lib.rs` (9,463 lines at `74d227f`) is the
main Phase 2 component. **It has not had a security review.** The property
results in §4 do not cover it.

Recent correctness fixes show why this contract needs its own review:

- `035ebc1` / `9ba37a0`: tick-crossing and burn bugs (#785, #786), merged via #807
- `cc1c262`: mint amount/liquidity mismatch and a swap precision underflow, merged via #784
- `70627c5`: swaps are now priced in Q64.96 to fix per-token insolvency, merged via #898
- `4c3ff82`: added a deadline parameter to `mint_position`, merged via #879
- `51d1a69`: new `swap_exact_out` pricing path using reverse tick-walking math

Each item below is an unreviewed surface. An item stays **Not reviewed** until
it has a manual review and findings recorded in this document.

| # | Surface | Functions (line at `74d227f`) | Status |
|---|---------|-------------------------------|--------|
| CL-1 | **Tick transitions**: bitmap flips, next/previous initialized tick lookup, updates to `liquidity_net` / `liquidity_gross`, and active-liquidity changes when a tick is crossed | `flip_tick` (3521), `next_initialized_tick` (3534), `update_tick` (3614), `get_tick` / `set_tick` (3593/3606), `simulate_tick_cross` (2022) | Not reviewed |
| CL-2 | **Price bounds**: tick ↔ sqrt-price conversion accuracy at `MIN_TICK`/`MAX_TICK` (±887,272, lines 23–24), `sqrt_price_limit_x96` handling (including the `0` "no limit" sentinel), and tick-spacing alignment | `tick_to_sqrt_price_x96` (1973), `sqrt_price_x96_to_tick` (1987), `current_sqrt_price_x96` (3690), `tick_to_price` (3369), limit handling in `swap` (2151–2162) and `walk_exact_out` (2634–2639) | Not reviewed |
| CL-3 | **Position ownership**: provider auth versus NFT token-id auth, legacy-path blocking after an NFT transfer, and NFT cleanup when a position closes | `burn_position` (1473), `burn_position_by_token_id` (1497), `collect_fees_by_token_id` (1546), `tokenize_position` (1755), `resolve_token_owner` (1782), `ensure_legacy_owner` (1803), `cleanup_nft_if_closed` (1829), `set_position_nft` (351) | Not reviewed |
| CL-4 | **Fee accounting**: global and per-tick fee growth, fee growth inside a range, pending-fee settlement on modify, burn and collect, and protocol-fee accrual and withdrawal | `fee_growth_inside` (3337), `fee_growth_below_helper` / `fee_growth_above_helper` (3654/3672), `pending_fees` (3505), `collect_fees_core` (1691), `set_protocol_fee` (477), `withdraw_protocol_fees` (498) | Not reviewed |
| CL-5 | **Liquidity-crossing arithmetic**: per-step amount computation, rounding direction, saturation and overflow in Q64.96, and conversions between liquidity and amounts | `compute_step` (3890), `compute_final_price_and_output` (3957), `compute_final_price_and_input` (2531), `u128_to_i128_saturating` (3935), `ceil_div` (2501), `liquidity_from_amounts` (3477), `amounts_for_liquidity_to_burn` (3441), `simulate_swap_walk` (3711) | Not reviewed |
| CL-6 | **Exact-out swaps**: the second pricing path, its consistency with `swap`, and the accuracy of its quotes | `swap_exact_out` (2796), `walk_exact_out` (2584), `quote_exact_out` (3025) | Not reviewed |
| CL-7 | **Position lifecycle**: mint, modify, single-sided mint, range orders, burn, and the solvency bound on burn and collect (contract balance ≥ total owed) | `mint_position` (640), `modify_position` (822), `mint_position_single_token` (986), `place_range_order` (1338), `check_range_order_filled` (1421), `burn_position_core` (1561) | Not reviewed |
| CL-8 | **Admin, pause and oracle controls**: two-step admin transfer, pause coverage across entry points, and oracle-deviation guard | `initialize` (259), `pause` / `unpause` (428/439), `propose_admin` / `accept_admin` (449/461), `set_oracle` (328), `set_max_oracle_deviation_bps` (408), `check_oracle_deviation` (595) | Not reviewed |
| CL-9 | **TWAP oracle**: tick-cumulative accumulation, observation interpolation, and behavior over long intervals | `get_tick_cumulative` (3161), `observe` (3179), `record_oracle_point` (3206), `oracle_cumulative_at` (3226) | Not reviewed |

---

## 4. Property-Based Testing Summary

The fuzz suite in `contracts/amm-fuzz/src/lib.rs` verified the following
constant-product properties over **10 000 random cases each**. These results
apply to `contracts/amm/src/lib.rs` only and are not evidence that the
concentrated-liquidity contract has the same invariant.

| Property | Result |
|----------|--------|
| Output < reserve_out for all valid inputs | PASS |
| Output ≥ 0 for all valid inputs | PASS |
| Output is monotone in amount_in | PASS |
| Effective rate is non-increasing as amount_in grows | PASS |
| Fee amount is bounded in [0, amount_in] | PASS |
| Zero-fee output equals pure CP formula (±1 rounding) | PASS |
| 100% fee yields zero output | PASS |
| k = reserve_in × reserve_out never decreases after a swap | PASS |
| get_amount_in is right-inverse of get_amount_out (±2 rounding) | PASS |

All regression cases pinned in the `regression` module also pass.

---

## 5. Out of Scope

- Gas / resource cost analysis (Soroban metering)
- Front-running / MEV (no mempool on Stellar)
- Network-level attacks
- Off-chain client code in `examples/`

---

## 6. Conclusion

The constant-product AMM review found the x*y=k invariant holding across the
fee tiers exercised by the fuzz suite. As of `74d227f`, all of the original
high, medium and low findings for that pool are fixed or acknowledged. One
medium finding (M-03, the residual initializer gap from M-02) and one
informational item (I-02) remain open.

The concentrated-liquidity contract has not been reviewed. The surfaces in §3
must be reviewed before that contract handles significant value. Read this
document as a scoped review of the constant-product pool, not as a claim that
the whole workspace is sound.
