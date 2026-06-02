//! V2 → V3 AMM Migration Contract (Issue #266)
//!
//! Atomically migrates a liquidity position from a V2 constant-product pool
//! to a V3 concentrated-liquidity pool in a single transaction.
//!
//! Flow:
//!   1. LP approves this contract to act on their behalf.
//!   2. LP calls `migrate` with their V2 LP share amount and desired V3 range.
//!   3. Contract burns V2 shares → receives token_a + token_b.
//!   4. Contract deposits into V3 pool at the computed optimal range.
//!   5. Any leftover tokens (due to range asymmetry) are returned to the LP.
//!   6. A migration-incentive fee discount is applied: the V3 deposit fee is
//!      waived for migrating LPs (enforced via a discount flag on the V3 pool).

#![no_std]

use soroban_sdk::{
    contract, contractclient, contractimpl, contracterror, contracttype, Address, Env,
};
use soroban_sdk::token::Client as TokenClient;

// ── Errors ────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum MigrationError {
    NotInitialized    = 1,
    AlreadyInitialized = 2,
    Unauthorized      = 3,
    ZeroShares        = 4,
    InvalidRange      = 5,
    SlippageExceeded  = 6,
    MigrationFailed   = 7,
}

// ── Storage keys ─────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    Admin,
    V2Pool,
    V3Pool,
}

// ── External interfaces ───────────────────────────────────────────────────────

/// Minimal V2 AMM interface needed for migration.
#[contractclient(name = "V2PoolClient")]
pub trait V2PoolInterface {
    fn remove_liquidity(
        env: Env,
        provider: Address,
        shares: i128,
        min_a: i128,
        min_b: i128,
        deadline: u64,
    ) -> Result<(i128, i128), soroban_sdk::Error>;

    fn get_info(env: Env) -> V2PoolInfo;
}

#[contracttype]
#[derive(Clone)]
pub struct V2PoolInfo {
    pub token_a: Address,
    pub token_b: Address,
    pub reserve_a: i128,
    pub reserve_b: i128,
    pub total_shares: i128,
    pub fee_bps: i128,
    pub flash_loan_fee_bps: i128,
    pub admin: Address,
    pub fee_recipient: Address,
    pub protocol_fee_bps: i128,
    pub lp_rebate_bps: i128,
}

/// Minimal V3 concentrated-liquidity interface needed for migration.
#[contractclient(name = "V3PoolClient")]
pub trait V3PoolInterface {
    /// Add liquidity within a price range [tick_lower, tick_upper].
    /// Returns the LP NFT position ID minted to `provider`.
    fn add_liquidity_range(
        env: Env,
        provider: Address,
        amount_a: i128,
        amount_b: i128,
        tick_lower: i32,
        tick_upper: i32,
        min_shares: i128,
        deadline: u64,
        fee_discount: bool,
    ) -> Result<i128, soroban_sdk::Error>;

    fn get_current_tick(env: Env) -> i32;
}

// ── Migration result ──────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, Debug)]
pub struct MigrationResult {
    /// V3 position ID (LP NFT) minted to the migrating LP.
    pub position_id: i128,
    /// Amount of token_a deposited into V3.
    pub deposited_a: i128,
    /// Amount of token_b deposited into V3.
    pub deposited_b: i128,
    /// Leftover token_a returned to the LP (range asymmetry dust).
    pub refund_a: i128,
    /// Leftover token_b returned to the LP.
    pub refund_b: i128,
    /// Optimal tick_lower computed for the V3 range.
    pub tick_lower: i32,
    /// Optimal tick_upper computed for the V3 range.
    pub tick_upper: i32,
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct MigrationContract;

#[contractimpl]
impl MigrationContract {
    // ── Setup ─────────────────────────────────────────────────────────────────

    /// Initialize the migration helper.
    ///
    /// # Parameters
    /// - `admin`   – Contract administrator.
    /// - `v2_pool` – Address of the V2 constant-product AMM pool.
    /// - `v3_pool` – Address of the V3 concentrated-liquidity pool.
    pub fn initialize(
        env: Env,
        admin: Address,
        v2_pool: Address,
        v3_pool: Address,
    ) -> Result<(), MigrationError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(MigrationError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::V2Pool, &v2_pool);
        env.storage().instance().set(&DataKey::V3Pool, &v3_pool);
        Ok(())
    }

    // ── Migration ─────────────────────────────────────────────────────────────

    /// Migrate a V2 LP position to V3 in a single atomic transaction.
    ///
    /// # Parameters
    /// - `provider`       – LP address; must authorise this call.
    /// - `v2_shares`      – Number of V2 LP tokens to burn.
    /// - `min_a`          – Minimum token_a to receive from V2 withdrawal (slippage).
    /// - `min_b`          – Minimum token_b to receive from V2 withdrawal (slippage).
    /// - `tick_lower`     – Desired lower tick for the V3 range.
    ///                      Pass `i32::MIN` to auto-compute an optimal range.
    /// - `tick_upper`     – Desired upper tick for the V3 range.
    ///                      Pass `i32::MAX` to auto-compute an optimal range.
    /// - `range_width_ticks` – Half-width of the auto-computed range (ignored when
    ///                      explicit ticks are provided).
    /// - `min_v3_shares`  – Minimum V3 position size (slippage guard on deposit).
    /// - `deadline`       – Latest ledger timestamp at which this call is valid.
    ///
    /// # Returns
    /// A [`MigrationResult`] describing what was deposited, the V3 position ID,
    /// and any dust refunded to the LP.
    #[allow(clippy::too_many_arguments)]
    pub fn migrate(
        env: Env,
        provider: Address,
        v2_shares: i128,
        min_a: i128,
        min_b: i128,
        tick_lower: i32,
        tick_upper: i32,
        range_width_ticks: i32,
        min_v3_shares: i128,
        deadline: u64,
    ) -> Result<MigrationResult, MigrationError> {
        if !env.storage().instance().has(&DataKey::Admin) {
            return Err(MigrationError::NotInitialized);
        }
        if v2_shares <= 0 {
            return Err(MigrationError::ZeroShares);
        }

        provider.require_auth();

        let v2_pool: Address = env.storage().instance().get(&DataKey::V2Pool).unwrap();
        let v3_pool: Address = env.storage().instance().get(&DataKey::V3Pool).unwrap();

        // ── Step 1: withdraw from V2 ─────────────────────────────────────────
        let v2_client = V2PoolClient::new(&env, &v2_pool);
        let pool_info = v2_client.get_info();
        let token_a = pool_info.token_a.clone();
        let token_b = pool_info.token_b.clone();

        // Remove liquidity from V2; tokens land in provider's wallet.
        let (received_a, received_b) =
            v2_client.remove_liquidity(&provider, &v2_shares, &min_a, &min_b, &deadline);

        // ── Step 2: compute optimal V3 tick range ────────────────────────────
        let v3_client = V3PoolClient::new(&env, &v3_pool);
        let (final_tick_lower, final_tick_upper) =
            Self::compute_range(&env, &v3_client, tick_lower, tick_upper, range_width_ticks)?;

        // ── Step 3: deposit into V3 with fee discount for migrating LPs ──────
        // Provider transfers tokens to this contract so we can forward them.
        let ta_client = TokenClient::new(&env, &token_a);
        let tb_client = TokenClient::new(&env, &token_b);
        let contract_addr = env.current_contract_address();

        ta_client.transfer(&provider, &contract_addr, &received_a);
        tb_client.transfer(&provider, &contract_addr, &received_b);

        // Approve V3 pool to pull from this contract.
        ta_client.approve(&contract_addr, &v3_pool, &received_a, &200u32);
        tb_client.approve(&contract_addr, &v3_pool, &received_b, &200u32);

        let position_id = v3_client.add_liquidity_range(
            &contract_addr,
            &received_a,
            &received_b,
            &final_tick_lower,
            &final_tick_upper,
            &min_v3_shares,
            &deadline,
            &true, // fee_discount: migration incentive
        );

        // ── Step 4: refund leftover dust to provider ──────────────────────────
        let refund_a = ta_client.balance(&contract_addr);
        let refund_b = tb_client.balance(&contract_addr);
        if refund_a > 0 {
            ta_client.transfer(&contract_addr, &provider, &refund_a);
        }
        if refund_b > 0 {
            tb_client.transfer(&contract_addr, &provider, &refund_b);
        }

        let deposited_a = received_a - refund_a;
        let deposited_b = received_b - refund_b;

        env.events().publish(
            (soroban_sdk::Symbol::new(&env, "migrated"), provider.clone()),
            (v2_shares, deposited_a, deposited_b, position_id, refund_a, refund_b),
        );

        Ok(MigrationResult {
            position_id,
            deposited_a,
            deposited_b,
            refund_a,
            refund_b,
            tick_lower: final_tick_lower,
            tick_upper: final_tick_upper,
        })
    }

    // ── Read-only helpers ─────────────────────────────────────────────────────

    /// Preview the optimal V3 tick range for a given V2 position without executing.
    ///
    /// Useful for off-chain UIs to show the user what range they'll get before
    /// they sign the migration transaction.
    pub fn preview_range(
        env: Env,
        tick_lower: i32,
        tick_upper: i32,
        range_width_ticks: i32,
    ) -> Result<(i32, i32), MigrationError> {
        if !env.storage().instance().has(&DataKey::Admin) {
            return Err(MigrationError::NotInitialized);
        }
        let v3_pool: Address = env.storage().instance().get(&DataKey::V3Pool).unwrap();
        let v3_client = V3PoolClient::new(&env, &v3_pool);
        Self::compute_range(&env, &v3_client, tick_lower, tick_upper, range_width_ticks)
    }

    // ── Internals ─────────────────────────────────────────────────────────────

    /// Compute the final [tick_lower, tick_upper] for the V3 deposit.
    ///
    /// If the caller passes explicit ticks (neither sentinel value), they are
    /// validated and returned as-is. Otherwise the current pool tick is fetched
    /// and a symmetric range of ±`range_width_ticks` is computed.
    fn compute_range(
        env: &Env,
        v3_client: &V3PoolClient,
        tick_lower: i32,
        tick_upper: i32,
        range_width_ticks: i32,
    ) -> Result<(i32, i32), MigrationError> {
        // Sentinel: caller wants auto-range.
        let auto = tick_lower == i32::MIN || tick_upper == i32::MAX;
        if auto {
            if range_width_ticks <= 0 {
                return Err(MigrationError::InvalidRange);
            }
            let current_tick = v3_client.get_current_tick();
            // Align to tick spacing of 1 (V3 implementations may enforce spacing;
            // callers should pass a width that is a multiple of their pool's spacing).
            let lower = current_tick - range_width_ticks;
            let upper = current_tick + range_width_ticks;
            return Ok((lower, upper));
        }
        // Explicit ticks: basic sanity check.
        if tick_lower >= tick_upper {
            return Err(MigrationError::InvalidRange);
        }
        let _ = env; // suppress unused warning
        Ok((tick_lower, tick_upper))
    }
}
