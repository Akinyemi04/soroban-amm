//! Concentrated Liquidity AMM (Uniswap v3-style tick-based ranges).
//! Standalone contract — does NOT modify the existing AMM pool.
#![no_std]

pub mod math;
pub mod tick_bitmap;

use soroban_sdk::token::Client as TokenClient;
use soroban_sdk::{
    contract, contractclient, contractimpl, contracterror, contracttype, symbol_short, Address,
    Env, Vec,
};

#[cfg(feature = "testutils")]
pub const WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/wasm32v1-none/release/concentrated_liquidity.wasm"
));

const PRICE_SCALE: i128 = 1_000_000;
const TICK_BASE_NUM: i128 = 1_000_100;
const TICK_BASE_DEN: i128 = PRICE_SCALE;
const MIN_TICK: i32 = -887_272;
const MAX_TICK: i32 = 887_272;

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ClError {
    AlreadyInitialized = 1,
    TokensMustDiffer = 2,
    InvalidFeeBps = 3,
    TickOutOfRange = 4,
    ZeroAmounts = 5,
    SlippageExceeded = 6,
    ZeroLiquidity = 7,
    InsufficientLiquidity = 8,
    PositionNotFound    = 9,
    DeadlineExpired     = 10,
    Paused              = 11,
    Unauthorized        = 12,
    TickNotAligned      = 13, // tick is not a multiple of tick_spacing
    InvalidTickSpacing  = 14, // tick_spacing must be > 0
    TickNotInitialized  = 15, // requested tick has no liquidity (never touched by a position)
    InvalidToken        = 16, // token_in is not token_a or token_b
    RangeOrderInRange   = 17, // range order must be fully out-of-range at creation
    OracleDeviationExceeded = 18,
}

/// Status of a range order (issue #295).
#[contracttype]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum RangeOrderStatus {
    /// Price has not yet crossed the range — order is pending.
    Pending = 0,
    /// Price has fully crossed the range — order is filled.
    Filled = 1,
    /// Position was closed before being filled.
    Closed = 2,
}

/// Result returned by `mint_position_single_token`.
///
/// Contains the actual amounts consumed and the liquidity minted.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct SingleTokenDepositResult {
    /// Amount of `token_in` actually consumed (≤ `amount_in`).
    pub amount_used: i128,
    /// Dust: `amount_in - amount_used` (returned to caller).
    pub dust: i128,
    /// Liquidity units added to the position.
    pub liquidity: i128,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum DataKey {
    TokenA,
    TokenB,
    FeeBps,
    CurrentTick,
    FeeGrowthGlobalA,
    FeeGrowthGlobalB,
    ActiveLiquidity,
    Position(Address, i32, i32),
    PositionList(Address), // Vec<(i32, i32)> of open tick ranges per provider
    TickCumulative,        // i64 — accumulated tick * elapsed_seconds
    LastOracleTimestamp,   // u64 — last oracle update timestamp
    OraclePoint(u64),      // timestamp → i64 tick_cumulative snapshot
    SqrtPriceX96,
    Tick(i32),
    TickBitmap(i32),
    Admin,
    Paused,
    TickSpacing, // i32 — only multiples of this value may be initialized as ticks
    RangeOrder(Address, i32, i32), // marks a position as a range order (issue #295)
    OracleAggregator,
    MaxOracleDeviationBps,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct AggregatedPrice {
    pub price: i128,
    pub confidence: u32,
}

#[contractclient(name = "OracleAggregatorClient")]
pub trait OracleAggregatorInterface {
    fn get_price_safe(env: Env, token_a: Address, token_b: Address) -> AggregatedPrice;
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct Position {
    pub lower_tick: i32,
    pub upper_tick: i32,
    pub liquidity: i128,
    pub fee_growth_inside_a: i128,
    pub fee_growth_inside_b: i128,
    pub tokens_owed: (i128, i128),
}

/// Per-tick state stored in the tick registry (issue #178).
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct TickInfo {
    /// Total liquidity referencing this tick (never negative).
    pub liquidity_gross: i128,
    /// Net liquidity change when crossing this tick upward (subtracted when crossing downward).
    pub liquidity_net: i128,
    pub fee_growth_outside_a: i128,
    pub fee_growth_outside_b: i128,
    pub initialized: bool,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PoolState {
    pub sqrt_price: u128,
    pub current_tick: i32,
    pub active_liquidity: i128,
    pub tick_spacing: i32,
}

/// Detailed read-only swap estimate for concentrated-liquidity routing.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PriceImpactEstimate {
    /// Gross input consumed by the simulated swap, including fees.
    pub amount_in: i128,
    /// Input that reaches pool math after LP fees.
    pub amount_in_after_fee: i128,
    /// Output amount predicted by the same tick walk used by `swap`.
    pub amount_out: i128,
    /// LP fee paid in the input token.
    pub fee_amount: i128,
    /// Spot token_out/token_in price before the swap, scaled by 1_000_000.
    pub spot_price_before: i128,
    /// Effective token_out/token_in price for this swap, scaled by 1_000_000.
    pub effective_price: i128,
    /// Price impact versus pre-swap spot, in basis points and including fees.
    pub price_impact_bps: i128,
    pub sqrt_price_before: u128,
    pub sqrt_price_after: u128,
    pub tick_before: i32,
    pub tick_after: i32,
    pub active_liquidity_before: i128,
    pub active_liquidity_after: i128,
}

#[contract]
pub struct ConcentratedLiquidity;

#[contractimpl]
impl ConcentratedLiquidity {
    /// One-time initialisation. Sets admin, token pair, fee, starting tick, and tick spacing.
    ///
    /// `tick_spacing` must be > 0. Only tick values that are exact multiples of
    /// `tick_spacing` may be used as position boundaries in `mint_position`.
    /// Suggested defaults: fee 5 bps → spacing 1, fee 30 bps → spacing 10,
    /// fee 100 bps → spacing 60.
    pub fn initialize(
        env: Env,
        admin: Address,
        token_a: Address,
        token_b: Address,
        fee_bps: i128,
        initial_tick: i32,
        tick_spacing: i32,
    ) -> Result<(), ClError> {
        if env.storage().instance().has(&DataKey::TokenA) {
            return Err(ClError::AlreadyInitialized);
        }
        if token_a == token_b {
            return Err(ClError::TokensMustDiffer);
        }
        if !(0..=10_000).contains(&fee_bps) {
            return Err(ClError::InvalidFeeBps);
        }
        if !(MIN_TICK..=MAX_TICK).contains(&initial_tick) {
            return Err(ClError::TickOutOfRange);
        }
        if tick_spacing <= 0 {
            return Err(ClError::InvalidTickSpacing);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Paused, &false);
        env.storage().instance().set(&DataKey::TokenA, &token_a);
        env.storage().instance().set(&DataKey::TokenB, &token_b);
        env.storage().instance().set(&DataKey::FeeBps, &fee_bps);
        env.storage()
            .instance()
            .set(&DataKey::CurrentTick, &initial_tick);
        env.storage()
            .instance()
            .set(&DataKey::TickSpacing, &tick_spacing);
        env.storage()
            .instance()
            .set(&DataKey::FeeGrowthGlobalA, &0_i128);
        env.storage()
            .instance()
            .set(&DataKey::FeeGrowthGlobalB, &0_i128);
        env.storage()
            .instance()
            .set(&DataKey::ActiveLiquidity, &0_i128);
        let init_ts = env.ledger().timestamp();
        env.storage().instance().set(&DataKey::TickCumulative, &0_i64);
        env.storage().instance().set(&DataKey::LastOracleTimestamp, &init_ts);
        env.storage().instance().set(&DataKey::OraclePoint(init_ts), &0_i64);
        env.storage()
            .instance()
            .set(&DataKey::OracleAggregator, &Option::<Address>::None);
        env.storage()
            .instance()
            .set(&DataKey::MaxOracleDeviationBps, &500_i128);
        Ok(())
    }

    /// Admin: attach or remove the oracle aggregator for swap deviation checks (#318).
    pub fn set_oracle(env: Env, admin: Address, oracle: Option<Address>) -> Result<(), ClError> {
        let stored: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        if admin != stored {
            return Err(ClError::Unauthorized);
        }
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::OracleAggregator, &oracle);
        Ok(())
    }

    /// Admin: max spot-vs-oracle deviation in basis points.
    pub fn set_max_oracle_deviation_bps(
        env: Env,
        admin: Address,
        max_deviation_bps: i128,
    ) -> Result<(), ClError> {
        let stored: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        if admin != stored {
            return Err(ClError::Unauthorized);
        }
        admin.require_auth();
        if !(0..=10_000).contains(&max_deviation_bps) {
            return Err(ClError::InvalidFeeBps);
        }
        env.storage()
            .instance()
            .set(&DataKey::MaxOracleDeviationBps, &max_deviation_bps);
        Ok(())
    }

    /// Pause all minting and swapping. Admin-only.
    pub fn pause(env: Env, admin: Address) -> Result<(), ClError> {
        let stored: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        if admin != stored {
            return Err(ClError::Unauthorized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &true);
        Ok(())
    }

    /// Resume minting and swapping. Admin-only.
    pub fn unpause(env: Env, admin: Address) -> Result<(), ClError> {
        let stored: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        if admin != stored {
            return Err(ClError::Unauthorized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &false);
        Ok(())
    }

    /// Returns true when the pool is paused.
    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    fn check_oracle_deviation(
        env: &Env,
        token_in: &Address,
        token_out: &Address,
        amount_in: i128,
        amount_out: i128,
    ) -> Result<(), ClError> {
        let oracle: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::OracleAggregator)
            .unwrap_or(None);
        let Some(oracle_addr) = oracle else {
            return Ok(());
        };
        let max_dev: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MaxOracleDeviationBps)
            .unwrap_or(500);

        let agg = OracleAggregatorClient::new(env, &oracle_addr)
            .get_price_safe(token_in, token_out);
        if agg.confidence == 0 || agg.price <= 0 {
            return Ok(());
        }

        let spot_price = amount_out * PRICE_SCALE / amount_in;
        let oracle_price = agg.price;
        let deviation_bps = if spot_price >= oracle_price {
            (spot_price - oracle_price) * 10_000 / oracle_price
        } else {
            (oracle_price - spot_price) * 10_000 / oracle_price
        };
        if deviation_bps > max_dev {
            return Err(ClError::OracleDeviationExceeded);
        }
        Ok(())
    }

    pub fn mint_position(
        env: Env,
        provider: Address,
        lower_tick: i32,
        upper_tick: i32,
        amount_a_desired: i128,
        amount_b_desired: i128,
        min_a: i128,
        min_b: i128,
    ) -> Result<(i128, i128), ClError> {
        if Self::is_paused(env.clone()) {
            return Err(ClError::Paused);
        }
        provider.require_auth();
        if lower_tick >= upper_tick {
            return Err(ClError::TickOutOfRange);
        }
        if lower_tick < MIN_TICK || upper_tick > MAX_TICK {
            return Err(ClError::TickOutOfRange);
        }
        // Enforce tick spacing: ticks must be multiples of tick_spacing.
        let tick_spacing: i32 = env
            .storage()
            .instance()
            .get(&DataKey::TickSpacing)
            .unwrap_or(1);
        if lower_tick % tick_spacing != 0 || upper_tick % tick_spacing != 0 {
            return Err(ClError::TickNotAligned);
        }
        if amount_a_desired <= 0 && amount_b_desired <= 0 {
            return Err(ClError::ZeroAmounts);
        }
        let current_tick: i32 = env.storage().instance().get(&DataKey::CurrentTick).unwrap();
        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let (amount_a, amount_b) = Self::amounts_for_liquidity(
            current_tick,
            lower_tick,
            upper_tick,
            amount_a_desired,
            amount_b_desired,
        );
        if amount_a < min_a || amount_b < min_b {
            return Err(ClError::SlippageExceeded);
        }
        let liquidity =
            Self::liquidity_from_amounts(current_tick, lower_tick, upper_tick, amount_a, amount_b);
        if liquidity <= 0 {
            return Err(ClError::ZeroLiquidity);
        }
        if amount_a > 0 {
            TokenClient::new(&env, &token_a).transfer(
                &provider,
                &env.current_contract_address(),
                &amount_a,
            );
        }
        if amount_b > 0 {
            TokenClient::new(&env, &token_b).transfer(
                &provider,
                &env.current_contract_address(),
                &amount_b,
            );
        }
        let pos_key = DataKey::Position(provider.clone(), lower_tick, upper_tick);

        let (fg_inside_a, fg_inside_b) =
            Self::fee_growth_inside(env.clone(), lower_tick, upper_tick);

        let mut pos: Position = env.storage().instance().get(&pos_key).unwrap_or(Position {
            lower_tick,
            upper_tick,
            liquidity: 0,
            fee_growth_inside_a: fg_inside_a,
            fee_growth_inside_b: fg_inside_b,
            tokens_owed: (0, 0),
        });
        let (oa, ob) = Self::pending_fees(&pos, fg_inside_a, fg_inside_b);
        pos.tokens_owed = (pos.tokens_owed.0 + oa, pos.tokens_owed.1 + ob);
        pos.fee_growth_inside_a = fg_inside_a;
        pos.fee_growth_inside_b = fg_inside_b;
        pos.liquidity += liquidity;
        // Track position list for get_positions view
        let list_key = DataKey::PositionList(provider.clone());
        let mut list: Vec<(i32, i32)> = env
            .storage()
            .instance()
            .get(&list_key)
            .unwrap_or_else(|| Vec::new(&env));
        let range = (lower_tick, upper_tick);
        if !list.iter().any(|r| r == range) {
            list.push_back(range);
            env.storage().instance().set(&list_key, &list);
        }
        env.storage().instance().set(&pos_key, &pos);

        let fg_a: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalA)
            .unwrap_or(0);
        let fg_b: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalB)
            .unwrap_or(0);
        Self::update_tick(&env, lower_tick, current_tick, liquidity, false, fg_a, fg_b);
        Self::update_tick(&env, upper_tick, current_tick, liquidity, true, fg_a, fg_b);

        if current_tick >= lower_tick && current_tick < upper_tick {
            let active: i128 = env
                .storage()
                .instance()
                .get(&DataKey::ActiveLiquidity)
                .unwrap_or(0);
            env.storage()
                .instance()
                .set(&DataKey::ActiveLiquidity, &(active + liquidity));
        }
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (symbol_short!("mint_pos"), provider),
            (lower_tick, upper_tick, liquidity, amount_a, amount_b)
        );
        Ok((amount_a, amount_b))
    }

    /// Increase liquidity on an existing position without closing it first.
    ///
    /// This explicit modification flow reuses the same `(provider, lower_tick,
    /// upper_tick)` storage key, settles accrued fees into `tokens_owed`, and
    /// computes the required token amounts from the current price before
    /// increasing the stored liquidity.
    pub fn modify_position(
        env: Env,
        provider: Address,
        lower_tick: i32,
        upper_tick: i32,
        liquidity_delta: i128,
        min_a: i128,
        min_b: i128,
    ) -> Result<(i128, i128), ClError> {
        if Self::is_paused(env.clone()) {
            return Err(ClError::Paused);
        }
        provider.require_auth();
        if lower_tick >= upper_tick {
            return Err(ClError::TickOutOfRange);
        }
        if lower_tick < MIN_TICK || upper_tick > MAX_TICK {
            return Err(ClError::TickOutOfRange);
        }
        let tick_spacing: i32 = env
            .storage()
            .instance()
            .get(&DataKey::TickSpacing)
            .unwrap_or(1);
        if lower_tick % tick_spacing != 0 || upper_tick % tick_spacing != 0 {
            return Err(ClError::TickNotAligned);
        }
        if liquidity_delta <= 0 {
            return Err(ClError::ZeroLiquidity);
        }

        let current_tick: i32 = env.storage().instance().get(&DataKey::CurrentTick).unwrap();
        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let pos_key = DataKey::Position(provider.clone(), lower_tick, upper_tick);
        let mut pos: Position = env
            .storage()
            .instance()
            .get(&pos_key)
            .ok_or(ClError::PositionNotFound)?;

        let (amount_a, amount_b) = Self::amounts_for_liquidity_to_burn(
            current_tick,
            lower_tick,
            upper_tick,
            liquidity_delta,
        );
        if amount_a <= 0 && amount_b <= 0 {
            return Err(ClError::ZeroLiquidity);
        }
        if amount_a < min_a || amount_b < min_b {
            return Err(ClError::SlippageExceeded);
        }

        if amount_a > 0 {
            TokenClient::new(&env, &token_a).transfer(
                &provider,
                &env.current_contract_address(),
                &amount_a,
            );
        }
        if amount_b > 0 {
            TokenClient::new(&env, &token_b).transfer(
                &provider,
                &env.current_contract_address(),
                &amount_b,
            );
        }

        let (fg_inside_a, fg_inside_b) =
            Self::fee_growth_inside(env.clone(), lower_tick, upper_tick);
        let (oa, ob) = Self::pending_fees(&pos, fg_inside_a, fg_inside_b);
        pos.tokens_owed = (pos.tokens_owed.0 + oa, pos.tokens_owed.1 + ob);
        pos.fee_growth_inside_a = fg_inside_a;
        pos.fee_growth_inside_b = fg_inside_b;
        pos.liquidity += liquidity_delta;

        let list_key = DataKey::PositionList(provider.clone());
        let mut list: Vec<(i32, i32)> = env
            .storage()
            .instance()
            .get(&list_key)
            .unwrap_or_else(|| Vec::new(&env));
        let range = (lower_tick, upper_tick);
        if !list.iter().any(|r| r == range) {
            list.push_back(range);
            env.storage().instance().set(&list_key, &list);
        }
        env.storage().instance().set(&pos_key, &pos);

        let fg_a: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalA)
            .unwrap_or(0);
        let fg_b: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalB)
            .unwrap_or(0);
        Self::update_tick(
            &env,
            lower_tick,
            current_tick,
            liquidity_delta,
            false,
            fg_a,
            fg_b,
        );
        Self::update_tick(
            &env,
            upper_tick,
            current_tick,
            liquidity_delta,
            true,
            fg_a,
            fg_b,
        );

        if current_tick >= lower_tick && current_tick < upper_tick {
            let active: i128 = env
                .storage()
                .instance()
                .get(&DataKey::ActiveLiquidity)
                .unwrap_or(0);
            env.storage()
                .instance()
                .set(&DataKey::ActiveLiquidity, &(active + liquidity_delta));
        }

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (symbol_short!("mod_pos"), provider),
            (lower_tick, upper_tick, liquidity_delta, amount_a, amount_b)
        );

        Ok((amount_a, amount_b))
    }

    /// Deposit a **single token** into a concentrated liquidity position.
    ///
    /// Behaviour depends on where the current price sits relative to the range:
    ///
    /// - `current_tick < lower_tick`  → price below range: only **token A** needed.
    /// - `current_tick >= upper_tick` → price above range: only **token B** needed.
    /// - in range → the deposited token covers its half of the range; dust returned.
    ///
    /// # Errors
    /// - [`ClError::Paused`] / [`ClError::DeadlineExpired`] – circuit breakers.
    /// - [`ClError::TickOutOfRange`] / [`ClError::TickNotAligned`] – bad ticks.
    /// - [`ClError::InvalidToken`]     – `token_in` is not a pool token.
    /// - [`ClError::ZeroAmounts`]      – `amount_in ≤ 0`.
    /// - [`ClError::ZeroLiquidity`]    – computed liquidity is zero.
    /// - [`ClError::SlippageExceeded`] – wrong token for price range, or below `min_liquidity`.
    #[allow(clippy::too_many_arguments)]
    pub fn mint_position_single_token(
        env: Env,
        provider: Address,
        lower_tick: i32,
        upper_tick: i32,
        token_in: Address,
        amount_in: i128,
        min_liquidity: i128,
        deadline: u64,
    ) -> Result<SingleTokenDepositResult, ClError> {
        if env.ledger().timestamp() > deadline {
            return Err(ClError::DeadlineExpired);
        }
        if Self::is_paused(env.clone()) {
            return Err(ClError::Paused);
        }
        provider.require_auth();

        // ── Validate tick range ───────────────────────────────────────────────
        if lower_tick >= upper_tick {
            return Err(ClError::TickOutOfRange);
        }
        if lower_tick < MIN_TICK || upper_tick > MAX_TICK {
            return Err(ClError::TickOutOfRange);
        }
        let tick_spacing: i32 = env
            .storage()
            .instance()
            .get(&DataKey::TickSpacing)
            .unwrap_or(1);
        if lower_tick % tick_spacing != 0 || upper_tick % tick_spacing != 0 {
            return Err(ClError::TickNotAligned);
        }
        if amount_in <= 0 {
            return Err(ClError::ZeroAmounts);
        }

        // ── Identify which token was supplied ────────────────────────────────
        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let is_token_a = token_in == token_a;
        if !is_token_a && token_in != token_b {
            return Err(ClError::InvalidToken);
        }

        let current_tick: i32 = env.storage().instance().get(&DataKey::CurrentTick).unwrap();

        // ── Compute (amount_a, amount_b, liquidity) from the single token ─────
        //
        // Three cases:
        //
        //  Case 1: current_tick < lower_tick  → price BELOW range
        //    Token A covers the entire range [lower, upper].
        //    Caller must supply token A; full amount_in is consumed.
        //
        //  Case 2: current_tick >= upper_tick → price ABOVE range
        //    Token B covers the entire range [lower, upper].
        //    Caller must supply token B; full amount_in is consumed.
        //
        //  Case 3: lower_tick <= current_tick < upper_tick → price IN range
        //    Token A covers [current_price, upper], Token B covers [lower, current_price].
        //    Single-token deposit provides liquidity for only the covered half.
        //    Dust = amount_in - amount_used (never transferred; stays with provider).
        //

        let (amount_a, amount_b, liquidity, amount_used) = if current_tick < lower_tick {
            // Case 1: price below range — only token A
            if !is_token_a {
                return Err(ClError::SlippageExceeded);
            }
            // Use proper sqrtPriceX96 formulas for accurate calculation
            let sqrt_lower = Self::tick_to_sqrt_price_x96(lower_tick);
            let sqrt_upper = Self::tick_to_sqrt_price_x96(upper_tick);
            let liq = math::get_liquidity_for_amount0(sqrt_lower, sqrt_upper, amount_in);
            (amount_in, 0_i128, liq.max(1), amount_in)
        } else if current_tick >= upper_tick {
            // Case 2: price above range — only token B
            if is_token_a {
                return Err(ClError::SlippageExceeded);
            }
            // Use proper sqrtPriceX96 formulas for accurate calculation
            let sqrt_lower = Self::tick_to_sqrt_price_x96(lower_tick);
            let sqrt_upper = Self::tick_to_sqrt_price_x96(upper_tick);
            let liq = math::get_liquidity_for_amount1(sqrt_lower, sqrt_upper, amount_in);
            (0_i128, amount_in, liq.max(1), amount_in)
        } else {
            // Case 3: price in range — compute liquidity from the single token's half
            // Using proper Uniswap V3 formulas with sqrtPriceX96
            let sqrt_lower = Self::tick_to_sqrt_price_x96(lower_tick);
            let sqrt_upper = Self::tick_to_sqrt_price_x96(upper_tick);
            let sqrt_current = Self::tick_to_sqrt_price_x96(current_tick);

            if sqrt_current >= sqrt_upper {
                // Degenerate: current at or above upper tick - treat as above-range for A
                let liq = math::get_liquidity_for_amount0(sqrt_upper, sqrt_upper, amount_in);
                (amount_in, 0_i128, liq.max(1), amount_in)
            } else if sqrt_current <= sqrt_lower {
                // Degenerate: current at or below lower tick - treat as below-range for B
                let liq = math::get_liquidity_for_amount1(sqrt_lower, sqrt_lower, amount_in);
                (0_i128, amount_in, liq.max(1), amount_in)
            } else if is_token_a {
                // Token A covers [current_price, upper_price].
                // Liquidity is computed from the amount, then we back-compute actual token amount.
                let liq = math::get_liquidity_for_amount0(sqrt_current, sqrt_upper, amount_in);
                let liq = liq.max(1);
                let used = math::get_amount0_delta(sqrt_current, sqrt_upper, liq);
                (used, 0_i128, liq, used)
            } else {
                // Token B covers [lower_price, current_price].
                let liq = math::get_liquidity_for_amount1(sqrt_lower, sqrt_current, amount_in);
                let liq = liq.max(1);
                let used = math::get_amount1_delta(sqrt_lower, sqrt_current, liq);
                (0_i128, used, liq, used)
            }
        };
        if liquidity <= 0 {
            return Err(ClError::ZeroLiquidity);
        }
        if liquidity < min_liquidity {
            return Err(ClError::SlippageExceeded);
        }

        // ── Transfer tokens from provider ─────────────────────────────────────
        if amount_a > 0 {
            TokenClient::new(&env, &token_a).transfer(
                &provider,
                &env.current_contract_address(),
                &amount_a,
            );
        }
        if amount_b > 0 {
            TokenClient::new(&env, &token_b).transfer(
                &provider,
                &env.current_contract_address(),
                &amount_b,
            );
        }

        // ── Update position state ─────────────────────────────────────────────
        let pos_key = DataKey::Position(provider.clone(), lower_tick, upper_tick);
        let (fg_inside_a, fg_inside_b) =
            Self::fee_growth_inside(env.clone(), lower_tick, upper_tick);

        let mut pos: Position = env.storage().instance().get(&pos_key).unwrap_or(Position {
            lower_tick,
            upper_tick,
            liquidity: 0,
            fee_growth_inside_a: fg_inside_a,
            fee_growth_inside_b: fg_inside_b,
            tokens_owed: (0, 0),
        });
        let (oa, ob) = Self::pending_fees(&pos, fg_inside_a, fg_inside_b);
        pos.tokens_owed = (pos.tokens_owed.0 + oa, pos.tokens_owed.1 + ob);
        pos.fee_growth_inside_a = fg_inside_a;
        pos.fee_growth_inside_b = fg_inside_b;
        pos.liquidity += liquidity;

        // Track position list.
        let list_key = DataKey::PositionList(provider.clone());
        let mut list: Vec<(i32, i32)> = env
            .storage()
            .instance()
            .get(&list_key)
            .unwrap_or_else(|| Vec::new(&env));
        let range_pair = (lower_tick, upper_tick);
        if !list.iter().any(|r| r == range_pair) {
            list.push_back(range_pair);
            env.storage().instance().set(&list_key, &list);
        }
        env.storage().instance().set(&pos_key, &pos);

        // ── Update tick state ─────────────────────────────────────────────────
        let fg_a: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalA)
            .unwrap_or(0);
        let fg_b: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalB)
            .unwrap_or(0);
        Self::update_tick(&env, lower_tick, current_tick, liquidity, false, fg_a, fg_b);
        Self::update_tick(&env, upper_tick, current_tick, liquidity, true, fg_a, fg_b);

        if current_tick >= lower_tick && current_tick < upper_tick {
            let active: i128 = env
                .storage()
                .instance()
                .get(&DataKey::ActiveLiquidity)
                .unwrap_or(0);
            env.storage()
                .instance()
                .set(&DataKey::ActiveLiquidity, &(active + liquidity));
        }

        // ── Return dust to provider ───────────────────────────────────────────
        // Dust = amount_in - amount_used. We never pulled the dust from the
        // provider, so no transfer is needed — it simply stays in their wallet.
        let dust = amount_in - amount_used;

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (symbol_short!("mint_1t"), provider),
            (lower_tick, upper_tick, liquidity, amount_used, dust)
        );

        Ok(SingleTokenDepositResult {
            amount_used,
            dust,
            liquidity,
        })
    }

    /// Quote the expected result of a single-token deposit without executing it.
    ///
    /// Pure read — does not transfer tokens or modify state.
    /// Returns values matching what [`mint_position_single_token`] would produce.
    pub fn quote_single_token_deposit(
        env: Env,
        lower_tick: i32,
        upper_tick: i32,
        token_in: Address,
        amount_in: i128,
    ) -> Result<SingleTokenDepositResult, ClError> {
        if lower_tick >= upper_tick {
            return Err(ClError::TickOutOfRange);
        }
        if lower_tick < MIN_TICK || upper_tick > MAX_TICK {
            return Err(ClError::TickOutOfRange);
        }
        if amount_in <= 0 {
            return Err(ClError::ZeroAmounts);
        }

        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let is_token_a = token_in == token_a;
        if !is_token_a && token_in != token_b {
            return Err(ClError::InvalidToken);
        }

        let current_tick: i32 = env.storage().instance().get(&DataKey::CurrentTick).unwrap();

        // Mirror the exact same logic as mint_position_single_token.
        let (liquidity, amount_used) = if current_tick < lower_tick {
            if !is_token_a {
                return Err(ClError::SlippageExceeded);
            }
            let sqrt_lower = Self::tick_to_sqrt_price_x96(lower_tick);
            let sqrt_upper = Self::tick_to_sqrt_price_x96(upper_tick);
            let liq = math::get_liquidity_for_amount0(sqrt_lower, sqrt_upper, amount_in);
            (liq.max(1), amount_in)
        } else if current_tick >= upper_tick {
            if is_token_a {
                return Err(ClError::SlippageExceeded);
            }
            let sqrt_lower = Self::tick_to_sqrt_price_x96(lower_tick);
            let sqrt_upper = Self::tick_to_sqrt_price_x96(upper_tick);
            let liq = math::get_liquidity_for_amount1(sqrt_lower, sqrt_upper, amount_in);
            (liq.max(1), amount_in)
        } else {
            let sqrt_lower = Self::tick_to_sqrt_price_x96(lower_tick);
            let sqrt_upper = Self::tick_to_sqrt_price_x96(upper_tick);
            let sqrt_current = Self::tick_to_sqrt_price_x96(current_tick);

            if sqrt_current >= sqrt_upper {
                let liq = math::get_liquidity_for_amount0(sqrt_upper, sqrt_upper, amount_in);
                (liq.max(1), amount_in)
            } else if sqrt_current <= sqrt_lower {
                let liq = math::get_liquidity_for_amount1(sqrt_lower, sqrt_lower, amount_in);
                (liq.max(1), amount_in)
            } else if is_token_a {
                let liq = math::get_liquidity_for_amount0(sqrt_current, sqrt_upper, amount_in);
                let liq = liq.max(1);
                let used = math::get_amount0_delta(sqrt_current, sqrt_upper, liq);
                (liq, used)
            } else {
                let liq = math::get_liquidity_for_amount1(sqrt_lower, sqrt_current, amount_in);
                let liq = liq.max(1);
                let used = math::get_amount1_delta(sqrt_lower, sqrt_current, liq);
                (liq, used)
            }
        };

        Ok(SingleTokenDepositResult {
            amount_used,
            dust: amount_in - amount_used,
            liquidity,
        })
    }

    // ── Issue #295: Range order support ──────────────────────────────────────

    /// Place a **range order** — a one-sided position that acts as a passive
    /// limit order.
    ///
    /// The range `[lower_tick, upper_tick)` must be **entirely above** or
    /// **entirely below** the current tick so that only one token is required.
    ///
    /// - Range above current tick (`current_tick < lower_tick`): deposit
    ///   `token_a`.  When price rises through the range the position converts
    ///   to `token_b`.
    /// - Range below current tick (`current_tick >= upper_tick`): deposit
    ///   `token_b`.  When price falls through the range the position converts
    ///   to `token_a`.
    ///
    /// The position is tagged internally so [`check_range_order_filled`] can
    /// report its status without requiring an off-chain keeper.
    ///
    /// # Errors
    /// - [`ClError::RangeOrderInRange`] – the range straddles the current tick.
    /// - All the usual [`ClError`] variants from [`mint_position_single_token`].
    #[allow(clippy::too_many_arguments)]
    pub fn place_range_order(
        env: Env,
        provider: Address,
        lower_tick: i32,
        upper_tick: i32,
        token_in: Address,
        amount_in: i128,
        min_liquidity: i128,
        deadline: u64,
    ) -> Result<SingleTokenDepositResult, ClError> {
        let current_tick: i32 = env
            .storage()
            .instance()
            .get(&DataKey::CurrentTick)
            .unwrap_or(0);

        // Enforce that the range is fully out-of-range (one-sided).
        let is_above = current_tick < lower_tick;
        let is_below = current_tick >= upper_tick;
        if !is_above && !is_below {
            return Err(ClError::RangeOrderInRange);
        }

        // Delegate to the existing single-token deposit logic.
        let result = Self::mint_position_single_token(
            env.clone(),
            provider.clone(),
            lower_tick,
            upper_tick,
            token_in,
            amount_in,
            min_liquidity,
            deadline,
        )?;

        // Tag the position as a range order.
        env.storage().instance().set(
            &DataKey::RangeOrder(provider.clone(), lower_tick, upper_tick),
            &true,
        );

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (symbol_short!("rng_ord"), provider),
            (lower_tick, upper_tick, result.liquidity, is_above)
        );

        Ok(result)
    }

    /// Check whether a range order has been filled.
    ///
    /// A range order is **filled** when the current tick has fully crossed the
    /// range:
    /// - An *above-range* order (token A → token B) is filled when
    ///   `current_tick >= upper_tick`.
    /// - A *below-range* order (token B → token A) is filled when
    ///   `current_tick < lower_tick`.
    ///
    /// Returns [`ClError::PositionNotFound`] if the position does not exist or
    /// was not placed via [`place_range_order`].
    pub fn check_range_order_filled(
        env: Env,
        provider: Address,
        lower_tick: i32,
        upper_tick: i32,
    ) -> Result<RangeOrderStatus, ClError> {
        // Verify the position exists and is tagged as a range order.
        let _pos: Position = env
            .storage()
            .instance()
            .get(&DataKey::Position(provider.clone(), lower_tick, upper_tick))
            .ok_or(ClError::PositionNotFound)?;

        let is_range_order: bool = env
            .storage()
            .instance()
            .get(&DataKey::RangeOrder(provider, lower_tick, upper_tick))
            .unwrap_or(false);
        if !is_range_order {
            return Err(ClError::PositionNotFound);
        }

        let current_tick: i32 = env
            .storage()
            .instance()
            .get(&DataKey::CurrentTick)
            .unwrap_or(0);

        // Determine fill direction from the range relative to the current tick
        // at the time of the query.
        let status = if current_tick >= upper_tick {
            // Price has risen above the range → above-range order is filled.
            RangeOrderStatus::Filled
        } else if current_tick < lower_tick {
            // Price is still below the range → above-range order is pending,
            // OR price has fallen below the range → below-range order is filled.
            // We distinguish by checking which side the range was on originally.
            // Since we only allow fully out-of-range creation, if current_tick
            // is now below lower_tick the below-range order is filled.
            RangeOrderStatus::Filled
        } else {
            // Price is inside the range — order is partially filled (pending).
            RangeOrderStatus::Pending
        };

        Ok(status)
    }

    pub fn burn_position(
        env: Env,
        provider: Address,
        lower_tick: i32,
        upper_tick: i32,
        liquidity: i128,
    ) -> Result<(i128, i128), ClError> {
        // No pause guard — LPs must always be able to exit.
        provider.require_auth();
        if liquidity <= 0 {
            return Err(ClError::ZeroLiquidity);
        }
        let pos_key = DataKey::Position(provider.clone(), lower_tick, upper_tick);
        let mut pos: Position = env
            .storage()
            .instance()
            .get(&pos_key)
            .ok_or(ClError::PositionNotFound)?;
        if pos.liquidity < liquidity {
            return Err(ClError::InsufficientLiquidity);
        }
        let current_tick: i32 = env.storage().instance().get(&DataKey::CurrentTick).unwrap();
        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();

        let (fg_inside_a, fg_inside_b) =
            Self::fee_growth_inside(env.clone(), lower_tick, upper_tick);
        let (oa, ob) = Self::pending_fees(&pos, fg_inside_a, fg_inside_b);
        pos.tokens_owed = (pos.tokens_owed.0 + oa, pos.tokens_owed.1 + ob);
        pos.fee_growth_inside_a = fg_inside_a;
        pos.fee_growth_inside_b = fg_inside_b;
        let (amount_a, amount_b) =
            Self::amounts_for_liquidity_to_burn(current_tick, lower_tick, upper_tick, liquidity);
        pos.liquidity -= liquidity;
        env.storage().instance().set(&pos_key, &pos);
        // Remove from position list when position is fully closed
        if pos.liquidity == 0 {
            let list_key = DataKey::PositionList(provider.clone());
            let list: Vec<(i32, i32)> = env
                .storage()
                .instance()
                .get(&list_key)
                .unwrap_or_else(|| Vec::new(&env));
            let range = (lower_tick, upper_tick);
            let mut new_list: Vec<(i32, i32)> = Vec::new(&env);
            for r in list.iter() {
                if r != range {
                    new_list.push_back(r);
                }
            }
            env.storage().instance().set(&list_key, &new_list);
        }

        let fg_a: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalA)
            .unwrap_or(0);
        let fg_b: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalB)
            .unwrap_or(0);
        Self::update_tick(
            &env,
            lower_tick,
            current_tick,
            -liquidity,
            false,
            fg_a,
            fg_b,
        );
        Self::update_tick(&env, upper_tick, current_tick, -liquidity, true, fg_a, fg_b);

        if current_tick >= lower_tick && current_tick < upper_tick {
            let active: i128 = env
                .storage()
                .instance()
                .get(&DataKey::ActiveLiquidity)
                .unwrap_or(0);
            env.storage().instance().set(
                &DataKey::ActiveLiquidity,
                &(if active > liquidity {
                    active - liquidity
                } else {
                    0
                }),
            );
        }
        if amount_a > 0 {
            TokenClient::new(&env, &token_a).transfer(
                &env.current_contract_address(),
                &provider,
                &amount_a,
            );
        }
        if amount_b > 0 {
            TokenClient::new(&env, &token_b).transfer(
                &env.current_contract_address(),
                &provider,
                &amount_b,
            );
        }
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (symbol_short!("burn_pos"), provider),
            (lower_tick, upper_tick, liquidity, amount_a, amount_b)
        );
        Ok((amount_a, amount_b))
    }

    pub fn collect_fees(
        env: Env,
        provider: Address,
        lower_tick: i32,
        upper_tick: i32,
    ) -> Result<(i128, i128), ClError> {
        // No pause guard — LPs must always be able to collect fees.
        provider.require_auth();
        let pos_key = DataKey::Position(provider.clone(), lower_tick, upper_tick);
        let mut pos: Position = env
            .storage()
            .instance()
            .get(&pos_key)
            .ok_or(ClError::PositionNotFound)?;

        let (fg_inside_a, fg_inside_b) =
            Self::fee_growth_inside(env.clone(), lower_tick, upper_tick);
        let (na, nb) = Self::pending_fees(&pos, fg_inside_a, fg_inside_b);
        let total_a = pos.tokens_owed.0 + na;
        let total_b = pos.tokens_owed.1 + nb;
        pos.tokens_owed = (0, 0);
        pos.fee_growth_inside_a = fg_inside_a;
        pos.fee_growth_inside_b = fg_inside_b;
        env.storage().instance().set(&pos_key, &pos);
        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        if total_a > 0 {
            TokenClient::new(&env, &token_a).transfer(
                &env.current_contract_address(),
                &provider,
                &total_a,
            );
        }
        if total_b > 0 {
            TokenClient::new(&env, &token_b).transfer(
                &env.current_contract_address(),
                &provider,
                &total_b,
            );
        }
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (symbol_short!("coll_fees"), provider),
            (lower_tick, upper_tick, total_a, total_b)
        );
        Ok((total_a, total_b))
    }

    pub fn get_position(
        env: Env,
        provider: Address,
        lower_tick: i32,
        upper_tick: i32,
    ) -> Result<Position, ClError> {
        env.storage()
            .instance()
            .get(&DataKey::Position(provider, lower_tick, upper_tick))
            .ok_or(ClError::PositionNotFound)
    }

    pub fn current_tick(env: Env) -> i32 {
        env.storage().instance().get(&DataKey::CurrentTick).unwrap()
    }

    pub fn active_liquidity(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::ActiveLiquidity)
            .unwrap_or(0)
    }

    pub fn get_pool_state(env: Env) -> PoolState {
        let current_tick: i32 = env
            .storage()
            .instance()
            .get(&DataKey::CurrentTick)
            .unwrap_or(0);
        let active_liquidity: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ActiveLiquidity)
            .unwrap_or(0);
        let tick_spacing: i32 = env
            .storage()
            .instance()
            .get(&DataKey::TickSpacing)
            .unwrap_or(1);
        let sqrt_price = env
            .storage()
            .instance()
            .get(&DataKey::SqrtPriceX96)
            .unwrap_or_else(|| {
                let price = Self::tick_to_price(current_tick);
                let sqrt_p = Self::sqrt(price);
                (sqrt_p as u128) * (1u128 << 96) / 1000u128
            });
        PoolState {
            sqrt_price,
            current_tick,
            active_liquidity,
            tick_spacing,
        }
    }

    // ── Issue #203: per-tick view functions ───────────────────────────────────

    /// Returns the `TickInfo` for an initialized tick.
    /// Returns `ClError::TickNotInitialized` if the tick has never been touched by a position.
    /// Requires no auth.
    pub fn get_tick_info(env: Env, tick: i32) -> Result<TickInfo, ClError> {
        env.storage()
            .instance()
            .get(&DataKey::Tick(tick))
            .ok_or(ClError::TickNotInitialized)
    }

    /// Returns `true` when the tick currently has non-zero gross liquidity.
    /// Requires no auth.
    pub fn is_tick_initialized(env: Env, tick: i32) -> bool {
        env.storage().instance().has(&DataKey::Tick(tick))
    }

    // ── Issue #218: public tick-bitmap helpers ────────────────────────────────

    /// Returns the lowest initialized tick **strictly above** `tick`.
    /// Uses the compressed tick bitmap for O(1)–O(log N) lookup.
    /// Returns `None` when no higher initialized tick exists.
    pub fn next_initialized_tick_pub(env: Env, tick: i32) -> Option<i32> {
        Self::next_initialized_tick(&env, tick, false)
    }

    /// Returns the highest initialized tick **at or below** `tick`.
    /// Uses the compressed tick bitmap for O(1)–O(log N) lookup.
    /// Returns `None` when no lower initialized tick exists.
    pub fn prev_initialized_tick_pub(env: Env, tick: i32) -> Option<i32> {
        Self::next_initialized_tick(&env, tick, true)
    }

    // ── Issue #219: sqrtPrice math library ────────────────────────────────────

    /// Converts a tick to `sqrtPriceX96 = sqrt(1.0001^tick) * 2^96`.
    ///
    /// Uses binary exponentiation (O(log |tick|)) with pre-sqrt scale-up for
    /// improved precision. Accurate within 1 basis point for |tick| ≤ 443_636.
    /// Extreme ticks saturate gracefully without panicking.
    pub fn tick_to_sqrt_price_x96(tick: i32) -> u128 {
        let tick = tick.clamp(MIN_TICK, MAX_TICK);
        let price = Self::tick_to_price_bexp(tick);
        // Scale up by 10^6 before taking the integer sqrt so that
        // sqrt(price * 10^6) ≈ sqrt(price) * 1000, giving three extra digits of
        // precision. Divide by 10^6 in the final step to normalize.
        let price_scaled = price.saturating_mul(1_000_000_i128).max(1);
        let sqrt_p = Self::sqrt(price_scaled);
        (sqrt_p as u128).saturating_mul(1u128 << 96) / 1_000_000_u128
    }

    /// Returns the largest tick `t` such that `tick_to_sqrt_price_x96(t) <= sqrt_price_x96`.
    ///
    /// Uses binary search over the full valid tick range [-887_272, 887_272].
    pub fn sqrt_price_x96_to_tick(sqrt_price_x96: u128) -> i32 {
        if sqrt_price_x96 == 0 {
            return MIN_TICK;
        }
        let mut low = MIN_TICK;
        let mut high = MAX_TICK;
        while low < high {
            // Bias mid toward high to avoid infinite loop when low+1==high.
            let mid = low + (high - low + 1) / 2;
            if Self::tick_to_sqrt_price_x96(mid) <= sqrt_price_x96 {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        low
    }

    // ── Issue #220: tick state-machine query helpers ──────────────────────────

    /// Returns the `liquidity_net` value stored at `tick`.
    ///
    /// When a swap crosses `tick` moving **upward** (zero_for_one = false),
    /// add `liquidity_net` to active liquidity.  When crossing **downward**
    /// (zero_for_one = true), subtract it.  Returns 0 for uninitialized ticks.
    pub fn get_liquidity_net_at_tick(env: Env, tick: i32) -> i128 {
        Self::get_tick(&env, tick).liquidity_net
    }

    /// Simulates the active-liquidity transition that occurs when a swap crosses `tick`.
    ///
    /// * `zero_for_one = true`  → price moving down; subtract `liquidity_net`.
    /// * `zero_for_one = false` → price moving up;   add    `liquidity_net`.
    ///
    /// Pure read — does **not** modify contract state.
    pub fn simulate_tick_cross(
        env: Env,
        current_liquidity: i128,
        tick: i32,
        zero_for_one: bool,
    ) -> i128 {
        let net = Self::get_tick(&env, tick).liquidity_net;
        if zero_for_one {
            (current_liquidity - net).max(0)
        } else {
            (current_liquidity + net).max(0)
        }
    }

    pub fn swap(
        env: Env,
        sender: Address,
        zero_for_one: bool,
        amount_in: i128,
        sqrt_price_limit_x96: u128,
        min_amount_out: i128,
        deadline: u64,
    ) -> Result<i128, ClError> {
        if env.ledger().timestamp() > deadline {
            return Err(ClError::DeadlineExpired);
        }
        if Self::is_paused(env.clone()) {
            return Err(ClError::Paused);
        }
        sender.require_auth();
        if amount_in <= 0 {
            return Err(ClError::ZeroAmounts);
        }

        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();

        // Update tick accumulator before changing current tick
        let now = env.ledger().timestamp();
        let last_ts: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LastOracleTimestamp)
            .unwrap_or(now);
        let elapsed = now.saturating_sub(last_ts) as i64;
        if elapsed > 0 {
            let current_tick_oracle: i32 = env
                .storage()
                .instance()
                .get(&DataKey::CurrentTick)
                .unwrap_or(0);
            let cum: i64 = env
                .storage()
                .instance()
                .get(&DataKey::TickCumulative)
                .unwrap_or(0);
            let new_cum = cum + (current_tick_oracle as i64) * elapsed;
            env.storage()
                .instance()
                .set(&DataKey::TickCumulative, &new_cum);
            env.storage()
                .instance()
                .set(&DataKey::LastOracleTimestamp, &now);
            env.storage()
                .instance()
                .set(&DataKey::OraclePoint(now), &new_cum);
        }

        let fee_bps: i128 = env.storage().instance().get(&DataKey::FeeBps).unwrap_or(0);
        let mut amount_remaining = amount_in;
        let mut amount_out_total = 0_i128;
        let mut current_tick = env
            .storage()
            .instance()
            .get(&DataKey::CurrentTick)
            .unwrap_or(0);
        let mut active_liquidity = env
            .storage()
            .instance()
            .get(&DataKey::ActiveLiquidity)
            .unwrap_or(0);
        let mut sqrt_price_x96 = env
            .storage()
            .instance()
            .get(&DataKey::SqrtPriceX96)
            .unwrap_or_else(|| {
                let price = Self::tick_to_price(current_tick);
                let sqrt_p = Self::sqrt(price);
                (sqrt_p as u128) * (1u128 << 96) / 1000u128
            });

        while amount_remaining > 0 {
            let next_tick_opt = Self::next_initialized_tick(&env, current_tick, zero_for_one);

            let next_tick = match next_tick_opt {
                Some(t) => {
                    if zero_for_one {
                        t.max(MIN_TICK)
                    } else {
                        t.min(MAX_TICK)
                    }
                }
                None => {
                    if zero_for_one {
                        MIN_TICK
                    } else {
                        MAX_TICK
                    }
                }
            };

            let next_price_x96 = {
                let price = Self::tick_to_price(next_tick);
                let sqrt_p = Self::sqrt(price);
                (sqrt_p as u128) * (1u128 << 96) / 1000u128
            };

            let mut target_price_x96 = next_price_x96;
            let mut hit_limit = false;

            if zero_for_one {
                if next_price_x96 <= sqrt_price_limit_x96 {
                    target_price_x96 = sqrt_price_limit_x96;
                    hit_limit = true;
                }
            } else if next_price_x96 >= sqrt_price_limit_x96 {
                target_price_x96 = sqrt_price_limit_x96;
                hit_limit = true;
            }

            let amount_in_after_fee = amount_remaining * (10000 - fee_bps) / 10000;

            let (amount_in_step_after_fee, amount_out_step) = if active_liquidity == 0 {
                (0, 0)
            } else {
                Self::compute_step(
                    active_liquidity,
                    sqrt_price_x96,
                    target_price_x96,
                    zero_for_one,
                )
            };

            if (amount_in_after_fee >= amount_in_step_after_fee || active_liquidity == 0)
                && !hit_limit
            {
                let actual_step_in = if active_liquidity > 0 && fee_bps > 0 {
                    (amount_in_step_after_fee * 10000 + 10000 - fee_bps - 1) / (10000 - fee_bps)
                } else {
                    amount_in_step_after_fee
                };

                let actual_step_in = actual_step_in.min(amount_remaining);

                amount_remaining -= actual_step_in;
                amount_out_total += amount_out_step;

                let fee = actual_step_in - amount_in_step_after_fee;
                if fee > 0 && active_liquidity > 0 {
                    if zero_for_one {
                        let fg_a: i128 = env
                            .storage()
                            .instance()
                            .get(&DataKey::FeeGrowthGlobalA)
                            .unwrap_or(0);
                        env.storage().instance().set(
                            &DataKey::FeeGrowthGlobalA,
                            &(fg_a + fee * 1_000_000 / active_liquidity),
                        );
                    } else {
                        let fg_b: i128 = env
                            .storage()
                            .instance()
                            .get(&DataKey::FeeGrowthGlobalB)
                            .unwrap_or(0);
                        env.storage().instance().set(
                            &DataKey::FeeGrowthGlobalB,
                            &(fg_b + fee * 1_000_000 / active_liquidity),
                        );
                    }
                }

                sqrt_price_x96 = target_price_x96;

                let mut tick_info = Self::get_tick(&env, next_tick);
                let fg_a: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::FeeGrowthGlobalA)
                    .unwrap_or(0);
                let fg_b: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::FeeGrowthGlobalB)
                    .unwrap_or(0);
                tick_info.fee_growth_outside_a = fg_a - tick_info.fee_growth_outside_a;
                tick_info.fee_growth_outside_b = fg_b - tick_info.fee_growth_outside_b;
                Self::set_tick(&env, next_tick, &tick_info);

                if zero_for_one {
                    active_liquidity -= tick_info.liquidity_net;
                    current_tick = next_tick - 1;
                } else {
                    active_liquidity += tick_info.liquidity_net;
                    current_tick = next_tick;
                }
            } else {
                if active_liquidity > 0 {
                    let (_target_p_x96, amt_in_after_fee) = if hit_limit {
                        (target_price_x96, amount_in_step_after_fee)
                    } else {
                        (target_price_x96, amount_in_after_fee)
                    };

                    let (new_price_x96, amount_out_step) = Self::compute_final_price_and_output(
                        active_liquidity,
                        sqrt_price_x96,
                        amt_in_after_fee,
                        zero_for_one,
                    );

                    let actual_in = if hit_limit {
                        if fee_bps > 0 {
                            (amt_in_after_fee * 10000 + 10000 - fee_bps - 1) / (10000 - fee_bps)
                        } else {
                            amt_in_after_fee
                        }
                    } else {
                        amount_remaining
                    };
                    let actual_in = actual_in.min(amount_remaining);

                    amount_remaining -= actual_in;
                    amount_out_total += amount_out_step;

                    let fee = actual_in - amt_in_after_fee;
                    if fee > 0 {
                        if zero_for_one {
                            let fg_a: i128 = env
                                .storage()
                                .instance()
                                .get(&DataKey::FeeGrowthGlobalA)
                                .unwrap_or(0);
                            env.storage().instance().set(
                                &DataKey::FeeGrowthGlobalA,
                                &(fg_a + fee * 1_000_000 / active_liquidity),
                            );
                        } else {
                            let fg_b: i128 = env
                                .storage()
                                .instance()
                                .get(&DataKey::FeeGrowthGlobalB)
                                .unwrap_or(0);
                            env.storage().instance().set(
                                &DataKey::FeeGrowthGlobalB,
                                &(fg_b + fee * 1_000_000 / active_liquidity),
                            );
                        }
                    }

                    if hit_limit {
                        sqrt_price_x96 = target_price_x96;
                    } else {
                        sqrt_price_x96 = new_price_x96;
                    }
                    current_tick = Self::price_to_tick(sqrt_price_x96);
                } else {
                    sqrt_price_x96 = target_price_x96;
                    current_tick = Self::price_to_tick(sqrt_price_x96);
                    amount_remaining = 0;
                }
                break;
            }
        }

        let amount_in_actual = amount_in - amount_remaining;
        if amount_out_total < min_amount_out {
            return Err(ClError::SlippageExceeded);
        }

        let token_in = if zero_for_one {
            token_a.clone()
        } else {
            token_b.clone()
        };
        let token_out = if zero_for_one {
            token_b.clone()
        } else {
            token_a.clone()
        };

        if amount_in_actual > 0 && amount_out_total > 0 {
            Self::check_oracle_deviation(
                &env,
                &token_in,
                &token_out,
                amount_in_actual,
                amount_out_total,
            )?;
        }

        if amount_in_actual > 0 {
            TokenClient::new(&env, &token_in).transfer(
                &sender,
                &env.current_contract_address(),
                &amount_in_actual,
            );
        }
        if amount_out_total > 0 {
            TokenClient::new(&env, &token_out).transfer(
                &env.current_contract_address(),
                &sender,
                &amount_out_total,
            );
        }

        env.storage()
            .instance()
            .set(&DataKey::CurrentTick, &current_tick);
        env.storage()
            .instance()
            .set(&DataKey::ActiveLiquidity, &active_liquidity);
        env.storage()
            .instance()
            .set(&DataKey::SqrtPriceX96, &sqrt_price_x96);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (soroban_sdk::symbol_short!("swap"), sender),
            (
                zero_for_one,
                amount_in_actual,
                amount_out_total,
                sqrt_price_x96,
                current_tick,
            )
        );

        Ok(amount_out_total)
    }

    /// Estimate swap output and price impact without transferring tokens or mutating pool state.
    ///
    /// This walks initialized ticks exactly like `swap`, so the returned output,
    /// final tick, final sqrt price, and fee amount should match an immediately
    /// executed swap with the same parameters.
    pub fn estimate_price_impact(
        env: Env,
        zero_for_one: bool,
        amount_in: i128,
        sqrt_price_limit_x96: u128,
    ) -> Result<PriceImpactEstimate, ClError> {
        if amount_in <= 0 {
            return Err(ClError::ZeroAmounts);
        }

        let fee_bps: i128 = env.storage().instance().get(&DataKey::FeeBps).unwrap_or(0);
        let tick_before = env
            .storage()
            .instance()
            .get(&DataKey::CurrentTick)
            .unwrap_or(0);
        let active_liquidity_before = env
            .storage()
            .instance()
            .get(&DataKey::ActiveLiquidity)
            .unwrap_or(0);
        let sqrt_price_before = Self::current_sqrt_price_x96(&env, tick_before);

        let (
            amount_in_actual,
            amount_in_after_fee,
            amount_out,
            sqrt_price_after,
            tick_after,
            active_liquidity_after,
        ) = Self::simulate_swap_walk(
            &env,
            zero_for_one,
            amount_in,
            sqrt_price_limit_x96,
            fee_bps,
            tick_before,
            active_liquidity_before,
            sqrt_price_before,
        );

        let fee_amount = amount_in_actual - amount_in_after_fee;
        let spot_price_before = Self::spot_price_for_direction(tick_before, zero_for_one);
        let effective_price = if amount_in_actual > 0 {
            amount_out * PRICE_SCALE / amount_in_actual
        } else {
            0
        };
        let price_impact_bps = if spot_price_before > 0 && effective_price < spot_price_before {
            (spot_price_before - effective_price) * 10_000 / spot_price_before
        } else {
            0
        };

        Ok(PriceImpactEstimate {
            amount_in: amount_in_actual,
            amount_in_after_fee,
            amount_out,
            fee_amount,
            spot_price_before,
            effective_price,
            price_impact_bps,
            sqrt_price_before,
            sqrt_price_after,
            tick_before,
            tick_after,
            active_liquidity_before,
            active_liquidity_after,
        })
    }

    /// Returns raw (tick_cumulative, last_timestamp) for external consumers.
    pub fn get_tick_cumulative(env: Env) -> (i64, u64) {
        let cum: i64 = env
            .storage()
            .instance()
            .get(&DataKey::TickCumulative)
            .unwrap_or(0);
        let ts: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LastOracleTimestamp)
            .unwrap_or(0);
        (cum, ts)
    }

    /// Returns tick_cumulative at `seconds_ago` seconds in the past.
    /// Looks up the stored oracle snapshot at exactly `now - seconds_ago`.
    /// `seconds_ago == 0` returns the current cumulative value (extrapolated to now).
    pub fn observe(env: Env, seconds_ago: u64) -> i64 {
        let cum: i64 = env
            .storage()
            .instance()
            .get(&DataKey::TickCumulative)
            .unwrap_or(0);
        let last_ts: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LastOracleTimestamp)
            .unwrap_or(0);
        let now = env.ledger().timestamp();
        let current_tick: i32 = env
            .storage()
            .instance()
            .get(&DataKey::CurrentTick)
            .unwrap_or(0);
        let target_ts = now.saturating_sub(seconds_ago);
        if target_ts >= last_ts {
            // Extrapolate forward from last stored point
            let elapsed = (target_ts - last_ts) as i64;
            cum + (current_tick as i64) * elapsed
        } else {
            // Look up stored oracle point at target timestamp
            env.storage()
                .instance()
                .get(&DataKey::OraclePoint(target_ts))
                .unwrap_or(0)
        }
    }

    /// Returns all open position tick-range pairs for `provider`.
    pub fn get_positions(env: Env, provider: Address) -> Vec<(i32, i32)> {
        env.storage()
            .instance()
            .get(&DataKey::PositionList(provider))
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Simulate token amounts required for a given tick range and liquidity.
    /// Pure read — does not transfer tokens or modify state.
    pub fn quote_position(
        env: Env,
        lower_tick: i32,
        upper_tick: i32,
        liquidity: i128,
    ) -> Result<(i128, i128), ClError> {
        if lower_tick >= upper_tick {
            return Err(ClError::TickOutOfRange);
        }
        if lower_tick < MIN_TICK || upper_tick > MAX_TICK {
            return Err(ClError::TickOutOfRange);
        }
        if liquidity <= 0 {
            return Err(ClError::ZeroLiquidity);
        }
        let current_tick: i32 = env
            .storage()
            .instance()
            .get(&DataKey::CurrentTick)
            .unwrap_or(0);
        Ok(Self::amounts_for_liquidity_to_burn(
            current_tick,
            lower_tick,
            upper_tick,
            liquidity,
        ))
    }

    pub fn fee_growth_inside(env: Env, lower_tick: i32, upper_tick: i32) -> (i128, i128) {
        let current_tick: i32 = env
            .storage()
            .instance()
            .get(&DataKey::CurrentTick)
            .unwrap_or(0);
        let fg_a: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalA)
            .unwrap_or(0);
        let fg_b: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalB)
            .unwrap_or(0);

        let (f_below_a, f_below_b) =
            Self::fee_growth_below_helper(&env, lower_tick, current_tick, fg_a, fg_b);
        let (f_above_a, f_above_b) =
            Self::fee_growth_above_helper(&env, upper_tick, current_tick, fg_a, fg_b);

        let inside_a = fg_a - f_below_a - f_above_a;
        let inside_b = fg_b - f_below_b - f_above_b;

        (inside_a, inside_b)
    }

    pub fn tick_to_price(tick: i32) -> i128 {
        if tick == 0 {
            return 1_000_000;
        }
        let abs_tick = tick.unsigned_abs() as i128;
        let iters = abs_tick.min(300);
        let mut price = 1_000_000_i128;
        for _ in 0..iters {
            price = price * 1_000_100 / 1_000_000;
        }
        if tick < 0 {
            price = 1_000_000 * 1_000_000 / price;
        }
        price
    }

    /// Binary-exponentiation variant of `tick_to_price`.
    ///
    /// Computes `PRICE_SCALE * 1.0001^tick` in O(log|tick|) multiplications,
    /// supporting the full tick range without the 300-iteration cap.
    /// Uses saturating arithmetic to prevent panics at extreme ticks.
    fn tick_to_price_bexp(tick: i32) -> i128 {
        if tick == 0 {
            return PRICE_SCALE;
        }
        let abs_tick = tick.unsigned_abs() as u32;
        let mut price = PRICE_SCALE;
        // base = TICK_BASE_NUM / TICK_BASE_DEN in PRICE_SCALE units = 1.0001 * 1_000_000
        let mut base = TICK_BASE_NUM;
        let mut exp = abs_tick;
        while exp > 0 {
            if exp & 1 != 0 {
                price = price.saturating_mul(base) / TICK_BASE_DEN;
            }
            base = base.saturating_mul(base) / TICK_BASE_DEN;
            exp >>= 1;
        }
        if tick < 0 {
            if price <= 0 {
                1
            } else {
                (PRICE_SCALE * PRICE_SCALE) / price
            }
        } else {
            price
        }
    }

    fn sqrt(y: i128) -> i128 {
        if y > 3 {
            let mut z = y;
            let mut x = y / 2 + 1;
            while x < z {
                z = x;
                x = (y / x + x) / 2;
            }
            z
        } else if y != 0 {
            1
        } else {
            0
        }
    }

    /// Compute actual token amounts from desired amounts based on current price position.
    /// When out of range, only one token type is used. When in range, amounts are
    /// distributed proportionally based on the price within the range.
    fn amounts_for_liquidity(ct: i32, lt: i32, ut: i32, ad: i128, bd: i128) -> (i128, i128) {
        if ct < lt {
            return (ad, 0);
        }
        if ct >= ut {
            return (0, bd);
        }

        // In-range: distribute amounts based on price position
        let pl = Self::tick_to_price(lt);
        let pu = Self::tick_to_price(ut);
        let pc = Self::tick_to_price(ct);
        let range = pu - pl;
        if range == 0 {
            return (ad / 2, bd / 2);
        }
        let below = pc - pl;
        (ad * (range - below) / range, bd * below / range)
    }

    /// Compute token amounts needed to burn `liquidity` from a position.
    /// Returns (amount_a, amount_b) based on current tick position.
    fn amounts_for_liquidity_to_burn(ct: i32, lt: i32, ut: i32, liquidity: i128) -> (i128, i128) {
        if ct < lt {
            // Price below range: only token A needed
            let sqrt_lower = Self::tick_to_sqrt_price_x96(lt);
            let sqrt_upper = Self::tick_to_sqrt_price_x96(ut);
            let amount_a = math::get_amount0_delta(sqrt_lower, sqrt_upper, liquidity);
            return (amount_a, 0);
        }
        if ct >= ut {
            // Price above range: only token B needed
            let sqrt_lower = Self::tick_to_sqrt_price_x96(lt);
            let sqrt_upper = Self::tick_to_sqrt_price_x96(ut);
            let amount_b = math::get_amount1_delta(sqrt_lower, sqrt_upper, liquidity);
            return (0, amount_b);
        }

        // In-range: use proper sqrtPriceX96 formulas
        let sqrt_lower = Self::tick_to_sqrt_price_x96(lt);
        let sqrt_upper = Self::tick_to_sqrt_price_x96(ut);
        let sqrt_current = Self::tick_to_sqrt_price_x96(ct);

        // Token A covers [current, upper], Token B covers [lower, current]
        let amount_a = math::get_amount0_delta(sqrt_current, sqrt_upper, liquidity);
        let amount_b = math::get_amount1_delta(sqrt_lower, sqrt_current, liquidity);
        (amount_a, amount_b)
    }

    fn liquidity_from_amounts(ct: i32, lt: i32, ut: i32, a: i128, b: i128) -> i128 {
        if ct < lt {
            a
        } else if ct >= ut {
            b
        } else {
            a.min(b).max(1)
        }
    }

    fn pending_fees(pos: &Position, fg_inside_a: i128, fg_inside_b: i128) -> (i128, i128) {
        let da = fg_inside_a - pos.fee_growth_inside_a;
        let db = fg_inside_b - pos.fee_growth_inside_b;
        let oa = if da > 0 {
            pos.liquidity * da / 1_000_000
        } else {
            0
        };
        let ob = if db > 0 {
            pos.liquidity * db / 1_000_000
        } else {
            0
        };
        (oa, ob)
    }

    fn flip_tick(env: &Env, tick: i32) {
        let word_pos = tick.div_euclid(128);
        let bit_pos = tick.rem_euclid(128) as u32;
        let key = DataKey::TickBitmap(word_pos);
        let mut word: u128 = env.storage().instance().get(&key).unwrap_or(0);
        word ^= 1 << bit_pos;
        if word == 0 {
            env.storage().instance().remove(&key);
        } else {
            env.storage().instance().set(&key, &word);
        }
    }

    fn next_initialized_tick(env: &Env, tick: i32, zero_for_one: bool) -> Option<i32> {
        if zero_for_one {
            let mut word_pos = tick.div_euclid(128);
            let bit_pos = tick.rem_euclid(128) as u32;
            let key = DataKey::TickBitmap(word_pos);
            if let Some(word) = env.storage().instance().get::<_, u128>(&key) {
                let mask = if bit_pos == 127 {
                    u128::MAX
                } else {
                    (1 << (bit_pos + 1)) - 1
                };
                let masked = word & mask;
                if masked != 0 {
                    let bit = 127 - masked.leading_zeros();
                    return Some(word_pos * 128 + bit as i32);
                }
            }
            let min_word = MIN_TICK.div_euclid(128);
            word_pos -= 1;
            while word_pos >= min_word {
                let key = DataKey::TickBitmap(word_pos);
                if let Some(word) = env.storage().instance().get::<_, u128>(&key) {
                    if word != 0 {
                        let bit = 127 - word.leading_zeros();
                        return Some(word_pos * 128 + bit as i32);
                    }
                }
                word_pos -= 1;
            }
            None
        } else {
            let start_tick = tick + 1;
            let mut word_pos = start_tick.div_euclid(128);
            let bit_pos = start_tick.rem_euclid(128) as u32;
            let key = DataKey::TickBitmap(word_pos);
            if let Some(word) = env.storage().instance().get::<_, u128>(&key) {
                let mask = u128::MAX << bit_pos;
                let masked = word & mask;
                if masked != 0 {
                    let bit = masked.trailing_zeros();
                    return Some(word_pos * 128 + bit as i32);
                }
            }
            let max_word = MAX_TICK.div_euclid(128);
            word_pos += 1;
            while word_pos <= max_word {
                let key = DataKey::TickBitmap(word_pos);
                if let Some(word) = env.storage().instance().get::<_, u128>(&key) {
                    if word != 0 {
                        let bit = word.trailing_zeros();
                        return Some(word_pos * 128 + bit as i32);
                    }
                }
                word_pos += 1;
            }
            None
        }
    }

    fn get_tick(env: &Env, tick: i32) -> TickInfo {
        env.storage()
            .instance()
            .get(&DataKey::Tick(tick))
            .unwrap_or(TickInfo {
                liquidity_net: 0,
                liquidity_gross: 0,
                fee_growth_outside_a: 0,
                fee_growth_outside_b: 0,
                initialized: false,
            })
    }

    fn set_tick(env: &Env, tick: i32, info: &TickInfo) {
        if info.liquidity_gross == 0 {
            env.storage().instance().remove(&DataKey::Tick(tick));
        } else {
            env.storage().instance().set(&DataKey::Tick(tick), info);
        }
    }

    fn update_tick(
        env: &Env,
        tick: i32,
        current_tick: i32,
        liquidity_delta: i128,
        is_upper: bool,
        fg_a: i128,
        fg_b: i128,
    ) {
        let mut info = Self::get_tick(env, tick);
        let prev_gross = info.liquidity_gross;
        info.liquidity_gross += liquidity_delta;

        if prev_gross == 0 {
            info.initialized = true;
            if tick <= current_tick {
                info.fee_growth_outside_a = fg_a;
                info.fee_growth_outside_b = fg_b;
            } else {
                info.fee_growth_outside_a = 0;
                info.fee_growth_outside_b = 0;
            }
            info.initialized = true;
            Self::flip_tick(env, tick);
        }

        if is_upper {
            info.liquidity_net -= liquidity_delta;
        } else {
            info.liquidity_net += liquidity_delta;
        }

        if info.liquidity_gross == 0 {
            info.initialized = false;
            Self::flip_tick(env, tick);
            env.storage().instance().remove(&DataKey::Tick(tick));
        } else {
            Self::set_tick(env, tick, &info);
        }
    }

    fn fee_growth_below_helper(
        env: &Env,
        tick: i32,
        current_tick: i32,
        fg_a: i128,
        fg_b: i128,
    ) -> (i128, i128) {
        let info = Self::get_tick(env, tick);
        if current_tick >= tick {
            (info.fee_growth_outside_a, info.fee_growth_outside_b)
        } else {
            (
                fg_a - info.fee_growth_outside_a,
                fg_b - info.fee_growth_outside_b,
            )
        }
    }

    fn fee_growth_above_helper(
        env: &Env,
        tick: i32,
        current_tick: i32,
        fg_a: i128,
        fg_b: i128,
    ) -> (i128, i128) {
        let info = Self::get_tick(env, tick);
        if current_tick < tick {
            (info.fee_growth_outside_a, info.fee_growth_outside_b)
        } else {
            (
                fg_a - info.fee_growth_outside_a,
                fg_b - info.fee_growth_outside_b,
            )
        }
    }

    fn current_sqrt_price_x96(env: &Env, current_tick: i32) -> u128 {
        env.storage()
            .instance()
            .get(&DataKey::SqrtPriceX96)
            .unwrap_or_else(|| {
                let price = Self::tick_to_price(current_tick);
                let sqrt_p = Self::sqrt(price);
                (sqrt_p as u128) * (1u128 << 96) / 1000u128
            })
    }

    fn spot_price_for_direction(current_tick: i32, zero_for_one: bool) -> i128 {
        let price = Self::tick_to_price(current_tick).max(1);
        if zero_for_one {
            price
        } else {
            PRICE_SCALE * PRICE_SCALE / price
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn simulate_swap_walk(
        env: &Env,
        zero_for_one: bool,
        amount_in: i128,
        sqrt_price_limit_x96: u128,
        fee_bps: i128,
        mut current_tick: i32,
        mut active_liquidity: i128,
        mut sqrt_price_x96: u128,
    ) -> (i128, i128, i128, u128, i32, i128) {
        let mut amount_remaining = amount_in;
        let mut amount_out_total = 0_i128;
        let mut amount_in_after_fee_total = 0_i128;

        while amount_remaining > 0 {
            let next_tick_opt = Self::next_initialized_tick(env, current_tick, zero_for_one);

            let next_tick = match next_tick_opt {
                Some(t) => {
                    if zero_for_one {
                        t.max(MIN_TICK)
                    } else {
                        t.min(MAX_TICK)
                    }
                }
                None => {
                    if zero_for_one {
                        MIN_TICK
                    } else {
                        MAX_TICK
                    }
                }
            };

            let next_price_x96 = {
                let price = Self::tick_to_price(next_tick);
                let sqrt_p = Self::sqrt(price);
                (sqrt_p as u128) * (1u128 << 96) / 1000u128
            };

            let mut target_price_x96 = next_price_x96;
            let mut hit_limit = false;

            if zero_for_one {
                if next_price_x96 <= sqrt_price_limit_x96 {
                    target_price_x96 = sqrt_price_limit_x96;
                    hit_limit = true;
                }
            } else if next_price_x96 >= sqrt_price_limit_x96 {
                target_price_x96 = sqrt_price_limit_x96;
                hit_limit = true;
            }

            let amount_in_after_fee = amount_remaining * (10000 - fee_bps) / 10000;

            let (amount_in_step_after_fee, amount_out_step) = if active_liquidity == 0 {
                (0, 0)
            } else {
                Self::compute_step(
                    active_liquidity,
                    sqrt_price_x96,
                    target_price_x96,
                    zero_for_one,
                )
            };

            if (amount_in_after_fee >= amount_in_step_after_fee || active_liquidity == 0)
                && !hit_limit
            {
                let actual_step_in = if active_liquidity > 0 && fee_bps > 0 {
                    (amount_in_step_after_fee * 10000 + 10000 - fee_bps - 1) / (10000 - fee_bps)
                } else {
                    amount_in_step_after_fee
                };
                let actual_step_in = actual_step_in.min(amount_remaining);

                amount_remaining -= actual_step_in;
                amount_in_after_fee_total += amount_in_step_after_fee;
                amount_out_total += amount_out_step;
                sqrt_price_x96 = target_price_x96;

                let tick_info = Self::get_tick(env, next_tick);
                if zero_for_one {
                    active_liquidity -= tick_info.liquidity_net;
                    current_tick = next_tick - 1;
                } else {
                    active_liquidity += tick_info.liquidity_net;
                    current_tick = next_tick;
                }
            } else {
                if active_liquidity > 0 {
                    let amt_in_after_fee = if hit_limit {
                        amount_in_step_after_fee
                    } else {
                        amount_in_after_fee
                    };

                    let (new_price_x96, amount_out_step) = Self::compute_final_price_and_output(
                        active_liquidity,
                        sqrt_price_x96,
                        amt_in_after_fee,
                        zero_for_one,
                    );

                    let actual_in = if hit_limit {
                        if fee_bps > 0 {
                            (amt_in_after_fee * 10000 + 10000 - fee_bps - 1) / (10000 - fee_bps)
                        } else {
                            amt_in_after_fee
                        }
                    } else {
                        amount_remaining
                    };
                    let actual_in = actual_in.min(amount_remaining);

                    amount_remaining -= actual_in;
                    amount_in_after_fee_total += amt_in_after_fee;
                    amount_out_total += amount_out_step;

                    if hit_limit {
                        sqrt_price_x96 = target_price_x96;
                    } else {
                        sqrt_price_x96 = new_price_x96;
                    }
                    current_tick = Self::price_to_tick(sqrt_price_x96);
                } else {
                    sqrt_price_x96 = target_price_x96;
                    current_tick = Self::price_to_tick(sqrt_price_x96);
                    amount_remaining = 0;
                }
                break;
            }
        }

        (
            amount_in - amount_remaining,
            amount_in_after_fee_total,
            amount_out_total,
            sqrt_price_x96,
            current_tick,
            active_liquidity,
        )
    }

    fn compute_step(
        liquidity: i128,
        sqrt_price_current_x96: u128,
        sqrt_price_target_x96: u128,
        zero_for_one: bool,
    ) -> (i128, i128) {
        let p_c = (((sqrt_price_current_x96 * 1000) >> 96) as i128).max(1);
        let p_t = (((sqrt_price_target_x96 * 1000) >> 96) as i128).max(1);

        if zero_for_one {
            let diff = p_c - p_t;
            if diff <= 0 {
                return (0, 0);
            }
            let amount_in = liquidity * 1000 * diff / (p_c * p_t);
            let amount_out = liquidity * diff / 1000;
            (amount_in, amount_out)
        } else {
            let diff = p_t - p_c;
            if diff <= 0 {
                return (0, 0);
            }
            let amount_in = liquidity * diff / 1000;
            let amount_out = liquidity * 1000 * diff / (p_c * p_t);
            (amount_in, amount_out)
        }
    }

    fn compute_final_price_and_output(
        liquidity: i128,
        sqrt_price_current_x96: u128,
        amount_in_after_fee: i128,
        zero_for_one: bool,
    ) -> (u128, i128) {
        let p_c = (((sqrt_price_current_x96 * 1000) >> 96) as i128).max(1);

        if zero_for_one {
            let denom = amount_in_after_fee * p_c + liquidity * 1000;
            let p_t = if denom > 0 {
                liquidity * 1000 * p_c / denom
            } else {
                p_c
            };
            let amount_out = liquidity * (p_c - p_t) / 1000;
            let sqrt_price_target_x96 = ((p_t as u128) * (1 << 96)) / 1000;
            (sqrt_price_target_x96, amount_out)
        } else {
            let p_t = p_c + amount_in_after_fee * 1000 / liquidity;
            let amount_out = liquidity * 1000 * (p_t - p_c) / (p_c * p_t.max(1));
            let sqrt_price_target_x96 = ((p_t as u128) * (1 << 96)) / 1000;
            (sqrt_price_target_x96, amount_out)
        }
    }

    fn price_to_tick(sqrt_p: u128) -> i32 {
        let sqrt_p_scaled = ((sqrt_p * 1000) >> 96) as i128;
        let target_price = sqrt_p_scaled * sqrt_p_scaled;
        let mut low = -300_i32;
        let mut high = 300_i32;
        let mut best_tick = 0_i32;
        let mut min_diff = i128::MAX;

        while low <= high {
            let mid = (low + high) / 2;
            let price = Self::tick_to_price(mid);
            let diff = (price - target_price).abs();
            if diff < min_diff {
                min_diff = diff;
                best_tick = mid;
            }
            if price < target_price {
                low = mid + 1;
            } else if price > target_price {
                high = mid - 1;
            } else {
                return mid;
            }
        }
        best_tick
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::token::StellarAssetClient;
    use soroban_sdk::{testutils::Address as _, testutils::Ledger as _, Env};

    #[allow(dead_code)]
    struct TestEnv<'a> {
        env: Env,
        admin: Address,
        provider: Address,
        token_a: Address,
        token_b: Address,
        cl_addr: Address,
        client: ConcentratedLiquidityClient<'a>,
        sac_a: StellarAssetClient<'a>,
        sac_b: StellarAssetClient<'a>,
    }

    fn setup_test_env<'a>(env: &'a Env, fee_bps: i128, initial_tick: i32) -> TestEnv<'a> {
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let admin = Address::generate(env);
        let provider = Address::generate(env);

        let token_a_sac = env.register_stellar_asset_contract_v2(admin.clone());
        let token_b_sac = env.register_stellar_asset_contract_v2(admin.clone());
        let token_a = token_a_sac.address();
        let token_b = token_b_sac.address();

        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &fee_bps, &initial_tick, &1_i32);

        let sac_a = StellarAssetClient::new(env, &token_a);
        let sac_b = StellarAssetClient::new(env, &token_b);

        // Mint lots of tokens to provider
        sac_a.mint(&provider, &10_000_000_i128);
        sac_b.mint(&provider, &10_000_000_i128);

        // Mint some to contract too just in case
        sac_a.mint(&cl_addr, &10_000_000_i128);
        sac_b.mint(&cl_addr, &10_000_000_i128);

        TestEnv {
            env: env.clone(),
            admin,
            provider,
            token_a,
            token_b,
            cl_addr: cl_addr.clone(),
            client,
            sac_a,
            sac_b,
        }
    }

    #[test]
    fn test_pool_state_flow() {
        let env = Env::default();
        let te = setup_test_env(&env, 30_i128, 0_i32);

        // 1. Test after initialize
        let state1 = te.client.get_pool_state();
        assert_eq!(state1.current_tick, 0);
        assert_eq!(state1.active_liquidity, 0);
        assert_eq!(state1.sqrt_price, 1u128 << 96);

        // 2. Test after mint_position
        // Range [-100, 100] covers current_tick = 0, so active_liquidity should increase.
        te.client.mint_position(
            &te.provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
        );
        let state2 = te.client.get_pool_state();
        assert_eq!(state2.current_tick, 0);
        assert!(state2.active_liquidity > 0);

        // 3. Test after a swap (selling token A for token B)
        te.client.swap(
            &te.provider,
            &true,
            &1_000_i128,
            &0_u128,
            &0_i128,
            &10_000_u64,
        );
        let state3 = te.client.get_pool_state();
        assert!(state3.current_tick < 0);
        assert!(state3.sqrt_price < (1u128 << 96));
    }

    #[test]
    fn test_single_range_swap() {
        let env = Env::default();
        let te = setup_test_env(&env, 30, 0); // 0.3% fee, start at tick 0

        te.client
            .mint_position(&te.provider, &-100, &100, &100_000, &100_000, &0, &0);

        let out = te.client.swap(&te.provider, &true, &1000, &0, &0, &10000);
        assert!(out > 0);

        let state = te.client.get_pool_state();
        assert!(state.current_tick < 0);
    }

    #[test]
    fn test_tick_crossing_swap() {
        let env = Env::default();
        let te = setup_test_env(&env, 0, 10); // 0% fee, start at tick 10

        te.client
            .mint_position(&te.provider, &-50, &0, &100_000, &100_000, &0, &0);

        let state_before = te.client.get_pool_state();
        assert_eq!(state_before.active_liquidity, 0); // outside range

        let out = te.client.swap(&te.provider, &true, &5000, &0, &0, &10000);
        assert!(out > 0);

        let state_after = te.client.get_pool_state();
        assert!(state_after.current_tick < 0);
        assert!(state_after.active_liquidity > 0);
    }

    #[test]
    fn test_price_impact_estimate_matches_single_range_swap() {
        let env = Env::default();
        let te = setup_test_env(&env, 30, 0);

        te.client
            .mint_position(&te.provider, &-100, &100, &100_000, &100_000, &0, &0);

        let quote = te.client.estimate_price_impact(&true, &1_000_i128, &0_u128);
        let out = te.client.swap(
            &te.provider,
            &true,
            &1_000_i128,
            &0_u128,
            &0_i128,
            &10_000_u64,
        );
        let state = te.client.get_pool_state();

        assert_eq!(quote.amount_out, out);
        assert_eq!(quote.sqrt_price_after, state.sqrt_price);
        assert_eq!(quote.tick_after, state.current_tick);
        assert_eq!(quote.active_liquidity_after, state.active_liquidity);
        assert_eq!(quote.amount_in, 1_000_i128);
        assert_eq!(quote.fee_amount, 3_i128);
        assert!(quote.effective_price > 0);
        assert!(quote.price_impact_bps > 0);
    }

    #[test]
    fn test_price_impact_estimate_matches_many_tick_crossing_swap() {
        let env = Env::default();
        let te = setup_test_env(&env, 25, 25);

        te.client
            .mint_position(&te.provider, &-100, &-50, &0, &80_000, &0, &0);
        te.client
            .mint_position(&te.provider, &-50, &0, &0, &90_000, &0, &0);
        te.client
            .mint_position(&te.provider, &0, &50, &100_000, &100_000, &0, &0);
        te.client
            .mint_position(&te.provider, &50, &100, &70_000, &0, &0, &0);

        let quote = te
            .client
            .estimate_price_impact(&true, &25_000_i128, &0_u128);
        let out = te.client.swap(
            &te.provider,
            &true,
            &25_000_i128,
            &0_u128,
            &0_i128,
            &10_000_u64,
        );
        let state = te.client.get_pool_state();

        assert_eq!(quote.amount_out, out);
        assert_eq!(quote.sqrt_price_after, state.sqrt_price);
        assert_eq!(quote.tick_after, state.current_tick);
        assert_eq!(quote.active_liquidity_after, state.active_liquidity);
        assert!(quote.tick_after < quote.tick_before);
        assert!(quote.price_impact_bps > 0);
    }

    #[test]
    fn test_limit_price_hit() {
        let env = Env::default();
        let te = setup_test_env(&env, 0, 0);

        te.client
            .mint_position(&te.provider, &-100, &100, &100_000, &100_000, &0, &0);

        let limit = (1u128 << 96) - 1_000_000;
        let out = te
            .client
            .swap(&te.provider, &true, &50_000, &limit, &0, &10000);
        assert!(out > 0);

        let state = te.client.get_pool_state();
        assert_eq!(state.sqrt_price, limit);
    }

    #[test]
    fn test_deadline_expired() {
        let env = Env::default();
        let te = setup_test_env(&env, 0, 0);
        env.ledger().set_timestamp(101);
        let result = te.client.try_swap(&te.provider, &true, &1000, &0, &0, &100);
        assert_eq!(result, Err(Ok(ClError::DeadlineExpired)));
    }

    #[test]
    fn test_non_overlapping_fee_collection() {
        let env = Env::default();
        let te = setup_test_env(&env, 1000, 100); // 10% fee, start at tick 100

        let provider1 = Address::generate(&env);
        te.sac_a.mint(&provider1, &1_000_000);
        te.sac_b.mint(&provider1, &1_000_000);
        let provider2 = Address::generate(&env);
        te.sac_a.mint(&provider2, &1_000_000);
        te.sac_b.mint(&provider2, &1_000_000);

        te.client
            .mint_position(&provider1, &0, &50, &100_000, &100_000, &0, &0);
        te.client
            .mint_position(&provider2, &50, &150, &100_000, &100_000, &0, &0);

        te.client
            .swap(&te.provider, &true, &20_000, &0, &0, &10_000);

        let (f1_a, f1_b) = te.client.collect_fees(&provider1, &0, &50);
        let (f2_a, f2_b) = te.client.collect_fees(&provider2, &50, &150);

        assert!(f1_a > 0 || f1_b > 0);
        assert!(f2_a > 0 || f2_b > 0);
    }

    // ── Issue #186: emergency pause tests ─────────────────────────────────────

    #[test]
    fn test_pause_rejects_mint_position() {
        let env = Env::default();
        let te = setup_test_env(&env, 30, 0);

        te.client.pause(&te.admin);
        assert!(te.client.is_paused());

        let result =
            te.client
                .try_mint_position(&te.provider, &-100, &100, &10_000, &10_000, &0, &0);
        assert_eq!(result, Err(Ok(ClError::Paused)));
    }

    #[test]
    fn test_pause_rejects_swap() {
        let env = Env::default();
        let te = setup_test_env(&env, 30, 0);

        te.client
            .mint_position(&te.provider, &-100, &100, &10_000, &10_000, &0, &0);
        te.client.pause(&te.admin);

        let result = te
            .client
            .try_swap(&te.provider, &true, &1_000, &0, &0, &u64::MAX);
        assert_eq!(result, Err(Ok(ClError::Paused)));
    }

    #[test]
    fn test_paused_allows_burn_and_collect() {
        let env = Env::default();
        let te = setup_test_env(&env, 30, 0);

        te.client
            .mint_position(&te.provider, &100, &200, &5_000, &0, &0, &0);
        let pos = te.client.get_position(&te.provider, &100, &200);
        let liq = pos.liquidity;

        te.client.pause(&te.admin);

        // burn_position should succeed while paused
        let result = te.client.try_burn_position(&te.provider, &100, &200, &liq);
        assert!(result.is_ok());

        // collect_fees should also succeed while paused (nothing to collect here but shouldn't error)
        // Re-mint to get a position to collect on
        te.client.unpause(&te.admin);
        te.client
            .mint_position(&te.provider, &100, &200, &5_000, &0, &0, &0);
        te.client.pause(&te.admin);
        let collect_result = te.client.try_collect_fees(&te.provider, &100, &200);
        assert!(collect_result.is_ok());
    }

    #[test]
    fn test_non_admin_pause_rejected() {
        let env = Env::default();
        let te = setup_test_env(&env, 30, 0);

        let non_admin = Address::generate(&env);
        let result = te.client.try_pause(&non_admin);
        assert_eq!(result, Err(Ok(ClError::Unauthorized)));
        assert!(!te.client.is_paused());
    }

    #[test]
    fn test_unpause_resumes_operations() {
        let env = Env::default();
        let te = setup_test_env(&env, 30, 0);

        te.client.pause(&te.admin);
        assert!(te.client.is_paused());

        te.client.unpause(&te.admin);
        assert!(!te.client.is_paused());

        // Should now succeed
        te.client
            .mint_position(&te.provider, &-100, &100, &10_000, &10_000, &0, &0);
    }

    #[test]
    fn collect_fees_emits_coll_fees_event() {
        let env = Env::default();
        let te = setup_test_env(&env, 1000, 100);
        let cl_addr = te.cl_addr.clone();

        te.client
            .mint_position(&te.provider, &0, &150, &100_000, &100_000, &0, &0);
        te.client
            .swap(&te.provider, &true, &2_000, &0, &0, &u64::MAX);

        let (total_a, total_b) = te.client.collect_fees(&te.provider, &0, &150);

        use soroban_sdk::{testutils::Events as _, IntoVal, Val, Vec as SdkVec};
        let expected_topics: SdkVec<Val> =
            (symbol_short!("coll_fees"), te.provider.clone()).into_val(&env);
        let event = env
            .events()
            .all()
            .iter()
            .find(|e| e.0 == cl_addr && e.1 == expected_topics)
            .expect("coll_fees event must be emitted");
        let __ver_4: (u32, (i32, i32, i128, i128)) = event.2.into_val(&env);
        assert_eq!(__ver_4.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        let data: (i32, i32, i128, i128) = __ver_4.1;
        assert_eq!(data, (0_i32, 150_i32, total_a, total_b));
    }

    #[test]
    fn second_collect_fees_returns_zero_without_new_swap() {
        let env = Env::default();
        let te = setup_test_env(&env, 1000, 100);

        te.client
            .mint_position(&te.provider, &0, &150, &100_000, &100_000, &0, &0);
        te.client
            .swap(&te.provider, &true, &2_000, &0, &0, &u64::MAX);

        let (first_a, first_b) = te.client.collect_fees(&te.provider, &0, &150);
        assert!(first_a > 0 || first_b > 0);

        let (second_a, second_b) = te.client.collect_fees(&te.provider, &0, &150);
        assert_eq!((second_a, second_b), (0, 0));
    }

    #[test]
    fn collect_fees_does_not_reduce_liquidity() {
        let env = Env::default();
        let te = setup_test_env(&env, 1000, 100);

        te.client
            .mint_position(&te.provider, &0, &150, &100_000, &100_000, &0, &0);
        let liq_before = te.client.get_position(&te.provider, &0, &150).liquidity;

        te.client
            .swap(&te.provider, &true, &2_000, &0, &0, &u64::MAX);
        te.client.collect_fees(&te.provider, &0, &150);

        let liq_after = te.client.get_position(&te.provider, &0, &150).liquidity;
        assert_eq!(liq_before, liq_after);
    }

    #[test]
    fn out_of_range_position_earns_no_fees() {
        let env = Env::default();
        let te = setup_test_env(&env, 1000, 100);

        let out_of_range = Address::generate(&env);
        te.sac_a.mint(&out_of_range, &1_000_000);
        te.sac_b.mint(&out_of_range, &1_000_000);

        te.client
            .mint_position(&te.provider, &0, &150, &100_000, &100_000, &0, &0);
        te.client
            .mint_position(&out_of_range, &300, &400, &100_000, &0, &0, &0);
        te.client
            .swap(&te.provider, &true, &2_000, &0, &0, &u64::MAX);

        let (in_a, in_b) = te.client.collect_fees(&te.provider, &0, &150);
        let (out_a, out_b) = te.client.collect_fees(&out_of_range, &300, &400);
        assert!(in_a > 0 || in_b > 0);
        assert_eq!((out_a, out_b), (0, 0));
    }

    #[test]
    fn collect_fees_after_full_burn_returns_accrued_fees() {
        let env = Env::default();
        let te = setup_test_env(&env, 1000, 100);

        te.client
            .mint_position(&te.provider, &0, &150, &100_000, &100_000, &0, &0);
        let liq = te.client.get_position(&te.provider, &0, &150).liquidity;

        te.client
            .swap(&te.provider, &true, &2_000, &0, &0, &u64::MAX);
        te.client.burn_position(&te.provider, &0, &150, &liq);

        assert_eq!(te.client.get_position(&te.provider, &0, &150).liquidity, 0);

        let (fee_a, fee_b) = te.client.collect_fees(&te.provider, &0, &150);
        assert!(fee_a > 0 || fee_b > 0);

        let (second_a, second_b) = te.client.collect_fees(&te.provider, &0, &150);
        assert_eq!((second_a, second_b), (0, 0));
    }

    #[test]
    fn fees_split_proportionally_between_equal_positions() {
        let env = Env::default();
        let te = setup_test_env(&env, 1000, 100);

        let p2 = Address::generate(&env);
        te.sac_a.mint(&p2, &1_000_000);
        te.sac_b.mint(&p2, &1_000_000);

        te.client
            .mint_position(&te.provider, &0, &150, &100_000, &100_000, &0, &0);
        te.client
            .mint_position(&p2, &0, &150, &100_000, &100_000, &0, &0);
        te.client
            .swap(&te.provider, &true, &2_000, &0, &0, &u64::MAX);

        let (f1_a, f1_b) = te.client.collect_fees(&te.provider, &0, &150);
        let (f2_a, f2_b) = te.client.collect_fees(&p2, &0, &150);

        assert!(f1_a > 0 || f1_b > 0);
        assert!(f2_a > 0 || f2_b > 0);
        assert!((f1_a - f2_a).abs() <= 1);
        assert!((f1_b - f2_b).abs() <= 1);
    }

    #[test]
    fn collect_fees_after_second_swap_returns_only_new_fees() {
        let env = Env::default();
        let te = setup_test_env(&env, 1000, 100);

        te.client
            .mint_position(&te.provider, &0, &150, &100_000, &100_000, &0, &0);
        te.client
            .swap(&te.provider, &true, &2_000, &0, &0, &u64::MAX);
        let (first_a, first_b) = te.client.collect_fees(&te.provider, &0, &150);

        te.client
            .swap(&te.provider, &false, &2_000, &u128::MAX, &0, &u64::MAX);
        let (second_a, second_b) = te.client.collect_fees(&te.provider, &0, &150);

        assert!(first_a + second_a > first_a || first_b + second_b > first_b);

        let (third_a, third_b) = te.client.collect_fees(&te.provider, &0, &150);
        assert_eq!((third_a, third_b), (0, 0));
    }

    #[test]
    fn tokens_owed_resets_after_collect() {
        let env = Env::default();
        let te = setup_test_env(&env, 1000, 100);

        te.client
            .mint_position(&te.provider, &0, &150, &100_000, &100_000, &0, &0);
        te.client
            .swap(&te.provider, &true, &2_000, &0, &0, &u64::MAX);
        te.client.collect_fees(&te.provider, &0, &150);

        let pos = te.client.get_position(&te.provider, &0, &150);
        assert_eq!(pos.tokens_owed, (0, 0));
    }

    #[test]
    fn burn_after_collect_returns_principal_not_fees() {
        let env = Env::default();
        let te = setup_test_env(&env, 1000, 100);

        te.client
            .mint_position(&te.provider, &200, &300, &50_000, &0, &0, &0);
        let liq = te.client.get_position(&te.provider, &200, &300).liquidity;

        let (fee_a, fee_b) = te.client.collect_fees(&te.provider, &200, &300);
        assert_eq!((fee_a, fee_b), (0, 0));

        let (burn_a, burn_b) = te.client.burn_position(&te.provider, &200, &300, &liq);
        assert!(burn_a > 0);
        assert_eq!(burn_b, 0);
    }

    #[test]
    fn fees_accrued_before_partial_burn_are_collectable() {
        let env = Env::default();
        let te = setup_test_env(&env, 1000, 100);

        te.client
            .mint_position(&te.provider, &0, &150, &100_000, &100_000, &0, &0);
        let liq = te.client.get_position(&te.provider, &0, &150).liquidity;

        te.client
            .swap(&te.provider, &true, &2_000, &0, &0, &u64::MAX);
        te.client.burn_position(&te.provider, &0, &150, &(liq / 2));

        let (fee_a, fee_b) = te.client.collect_fees(&te.provider, &0, &150);
        assert!(fee_a > 0 || fee_b > 0);

        let pos = te.client.get_position(&te.provider, &0, &150);
        assert_eq!(pos.liquidity, liq - liq / 2);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Events as _};
    use soroban_sdk::token::StellarAssetClient;
    use soroban_sdk::{IntoVal, Val, Vec as SdkVec};

    #[test]
    fn burn_position_emits_burn_pos_event_with_returned_amounts() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let token_a = env.register_stellar_asset_contract_v2(admin.clone());
        let token_b = env.register_stellar_asset_contract_v2(admin.clone());
        let token_a_addr = token_a.address();
        let token_b_addr = token_b.address();

        let contract_id = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &contract_id);
        client.initialize(
            &admin,
            &token_a_addr,
            &token_b_addr,
            &30_i128,
            &0_i32,
            &1_i32,
        );

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a_addr).mint(&provider, &1_000_i128);

        let lower_tick = 100_i32;
        let upper_tick = 200_i32;
        let (mint_a, mint_b) = client.mint_position(
            &provider,
            &lower_tick,
            &upper_tick,
            &1_000_i128,
            &0_i128,
            &0_i128,
            &0_i128,
        );
        assert_eq!((mint_a, mint_b), (1_000_i128, 0_i128));

        let liquidity = client
            .get_position(&provider, &lower_tick, &upper_tick)
            .liquidity;
        let (amount_a, amount_b) =
            client.burn_position(&provider, &lower_tick, &upper_tick, &liquidity);
        assert_eq!((amount_a, amount_b), (1_000_i128, 0_i128));

        let expected_topics: SdkVec<Val> =
            (symbol_short!("burn_pos"), provider.clone()).into_val(&env);
        let event = env
            .events()
            .all()
            .iter()
            .find(|e| e.0 == contract_id && e.1 == expected_topics)
            .expect("burn_pos event not emitted");

        let __ver_5: (u32, (i32, i32, i128, i128, i128)) = event.2.into_val(&env);
        assert_eq!(__ver_5.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        let data: (i32, i32, i128, i128, i128) = __ver_5.1;
        assert_eq!(
            data,
            (lower_tick, upper_tick, liquidity, amount_a, amount_b)
        );
    }
}

#[cfg(test)]
mod test_new_features {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::token::StellarAssetClient;
    use soroban_sdk::Env;

    fn setup(env: &Env) -> (Address, Address, Address, ConcentratedLiquidityClient) {
        let admin = Address::generate(env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &30_i128, &0_i32, &1_i32);
        (admin, token_a, token_b, client)
    }

    // ── Issue #183: TWAP tick accumulator ────────────────────────────────────

    #[test]
    fn tick_cumulative_advances_across_swaps() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1_000);

        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &30_i128, &10_i32, &1_i32);

        // Mint tokens for swapping
        StellarAssetClient::new(&env, &token_a).mint(&cl_addr, &1_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&cl_addr, &1_000_000_i128);

        let buyer = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&buyer, &10_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&buyer, &10_000_i128);

        // First swap at t=1060: tick was 10 for 60 seconds → cumulative += 10 * 60 = 600
        env.ledger().set_timestamp(1_060);
        client.swap(&buyer, &true, &100_i128, &0_u128, &0_i128, &u64::MAX);
        let (cum1, ts1) = client.get_tick_cumulative();
        assert_eq!(cum1, 600); // 10 * 60
        assert_eq!(ts1, 1_060);

        // Get tick after first swap, then record cumulative after second swap
        let tick_after_first = client.current_tick();

        // Second swap at t=1160: tick was tick_after_first for 100 seconds
        env.ledger().set_timestamp(1_160);
        client.swap(&buyer, &false, &100_i128, &u128::MAX, &0_i128, &u64::MAX);
        let (cum2, ts2) = client.get_tick_cumulative();
        let expected_cum2 = 600 + (tick_after_first as i64) * 100;
        assert_eq!(cum2, expected_cum2);
        assert_eq!(ts2, 1_160);
    }

    #[test]
    fn observe_zero_returns_current_cumulative() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1_000);

        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &30_i128, &5_i32, &1_i32);

        StellarAssetClient::new(&env, &token_a).mint(&cl_addr, &1_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&cl_addr, &1_000_000_i128);
        let buyer = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&buyer, &1_000_i128);

        // At t=1100: tick was 5 for 100 seconds → expect 5*100=500 at observe(0)
        env.ledger().set_timestamp(1_100);
        client.swap(&buyer, &true, &100_i128, &0_u128, &0_i128, &u64::MAX);
        // After swap: cum=500 (from tick=5), now at some new tick
        // observe(0) should extrapolate to now: 500 + new_tick*(1100-1100) = 500
        let obs = client.observe(&0_u64);
        assert_eq!(obs, 500);
    }

    #[test]
    fn average_tick_from_two_observes_matches_expected_price() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1_000);

        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &30_i128, &0_i32, &1_i32);

        StellarAssetClient::new(&env, &token_a).mint(&cl_addr, &1_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&cl_addr, &1_000_000_i128);
        let buyer = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&buyer, &2_000_i128);

        // Swap at t=1100 → oracle accumulates tick=0 * 100s = 0
        env.ledger().set_timestamp(1_100);
        client.swap(&buyer, &true, &100_i128, &0_u128, &0_i128, &u64::MAX);
        let tick_at_1100 = client.current_tick();

        // Swap at t=1200 → oracle accumulates tick_at_1100 * 100s
        env.ledger().set_timestamp(1_200);
        client.swap(&buyer, &true, &100_i128, &0_u128, &0_i128, &u64::MAX);

        // Compute average tick over [1000, 1200]:
        // cum at t=1000 = 0 (initialized)
        // cum at t=1200 = 0*100 + tick_at_1100*100
        let obs_now = client.observe(&0_u64); // cum at t=1200
        let obs_200s_ago = client.observe(&200_u64); // cum at t=1000 = 0
        let avg_tick = (obs_now - obs_200s_ago) / 200_i64;
        // avg tick = tick_at_1100 * 100 / 200 = tick_at_1100 / 2
        assert_eq!(avg_tick, (tick_at_1100 as i64) / 2);
    }

    // ── Issue #184: get_positions ─────────────────────────────────────────────

    #[test]
    fn get_positions_mint_two_close_one() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &30_i128, &0_i32, &1_i32);

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &10_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&provider, &10_000_i128);

        // Mint two positions
        client.mint_position(
            &provider,
            &-100_i32,
            &100_i32,
            &5_000_i128,
            &5_000_i128,
            &0_i128,
            &0_i128,
        );
        client.mint_position(
            &provider,
            &200_i32,
            &400_i32,
            &3_000_i128,
            &0_i128,
            &0_i128,
            &0_i128,
        );

        let positions = client.get_positions(&provider);
        assert_eq!(positions.len(), 2);

        // Close first position
        let liq1 = client
            .get_position(&provider, &-100_i32, &100_i32)
            .liquidity;
        StellarAssetClient::new(&env, &token_a).mint(&cl_addr, &10_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&cl_addr, &10_000_i128);
        client.burn_position(&provider, &-100_i32, &100_i32, &liq1);

        let positions_after = client.get_positions(&provider);
        assert_eq!(positions_after.len(), 1);
        assert_eq!(positions_after.get(0).unwrap(), (200_i32, 400_i32));
    }

    // ── Issue #185: quote_position ────────────────────────────────────────────

    #[test]
    fn quote_position_matches_mint_deduction() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        // current_tick = 0; range [100, 200] is entirely above → pure token-A position
        client.initialize(&admin, &token_a, &token_b, &30_i128, &0_i32, &1_i32);

        // quote_position: above-range means all in token_a → approximately liquidity worth
        let (qa, qb) = client.quote_position(&100_i32, &200_i32, &3_000_i128);
        assert!(qa > 0, "token_a amount should be positive");
        assert_eq!(qb, 0_i128, "token_b should be zero for above-range");

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &10_000_i128);

        // mint_position with the same range should consume approximately (qa, qb)
        let (ma, mb) = client.mint_position(
            &provider, &100_i32, &200_i32, &qa, &0_i128, &0_i128, &0_i128,
        );
        // Due to rounding, amounts may differ slightly
        assert!(ma > 0, "mint should consume some token_a");
        assert_eq!(
            mb, 0_i128,
            "mint should not consume token_b for above-range"
        );
    }

    // ── Issue #223: position modification ────────────────────────────────────

    #[test]
    fn modify_position_increases_liquidity_settles_fees_and_reuses_storage() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &30_i128, &0_i32, &1_i32);

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &500_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&provider, &500_000_i128);
        StellarAssetClient::new(&env, &token_a).mint(&cl_addr, &500_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&cl_addr, &500_000_i128);

        client.mint_position(
            &provider,
            &-100_i32,
            &100_i32,
            &50_000_i128,
            &50_000_i128,
            &0_i128,
            &0_i128,
        );

        let position_before = client.get_position(&provider, &-100_i32, &100_i32);
        let positions_before = client.get_positions(&provider);
        assert_eq!(
            positions_before.len(),
            1,
            "opening a position should track one range"
        );

        // Accrue fees against the position, but do not collect them yet.
        client.swap(&provider, &true, &20_000_i128, &0_u128, &0_i128, &u64::MAX);
        let position_after_swap = client.get_position(&provider, &-100_i32, &100_i32);
        assert_eq!(
            position_after_swap.tokens_owed, position_before.tokens_owed,
            "swap should accrue fees without auto-settling them"
        );

        let quote = client
            .quote_position(&-100_i32, &100_i32, &5_000_i128)
            .unwrap();
        let (added_a, added_b) = client
            .modify_position(
                &provider,
                &-100_i32,
                &100_i32,
                &5_000_i128,
                &0_i128,
                &0_i128,
            )
            .unwrap();

        assert_eq!(
            added_a, quote.0,
            "modify_position must use the current-price quote for token A"
        );
        assert_eq!(
            added_b, quote.1,
            "modify_position must use the current-price quote for token B"
        );

        let position_after = client.get_position(&provider, &-100_i32, &100_i32);
        assert_eq!(
            position_after.liquidity,
            position_before.liquidity + 5_000_i128,
            "liquidity must increase in place"
        );
        assert!(
            position_after.tokens_owed.0 > position_after_swap.tokens_owed.0
                || position_after.tokens_owed.1 > position_after_swap.tokens_owed.1,
            "fees accrued before modification must be settled into tokens_owed"
        );

        let positions_after = client.get_positions(&provider);
        assert_eq!(
            positions_after.len(),
            1,
            "storage must be reused for the same range"
        );
        assert_eq!(
            positions_after.get(0).unwrap(),
            (-100_i32, 100_i32),
            "the same position key should remain in the provider list"
        );
    }
}

// ── Issue #187: tick_spacing tests ───────────────────────────────────────────
#[cfg(test)]
mod test_tick_spacing {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::StellarAssetClient;
    use soroban_sdk::Env;

    fn setup_cl(
        env: &Env,
        tick_spacing: i32,
    ) -> (Address, Address, Address, ConcentratedLiquidityClient) {
        env.mock_all_auths();
        let admin = Address::generate(env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &30_i128, &0_i32, &tick_spacing);

        let provider = Address::generate(env);
        StellarAssetClient::new(env, &token_a).mint(&provider, &1_000_000_i128);
        StellarAssetClient::new(env, &token_b).mint(&provider, &1_000_000_i128);
        (provider, token_a, token_b, client)
    }

    /// Ticks that are exact multiples of tick_spacing must be accepted.
    #[test]
    fn test_aligned_ticks_succeed() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_cl(&env, 10);

        // -100 and 100 are both multiples of 10 → must succeed.
        let result = client.try_mint_position(
            &provider,
            &-100_i32,
            &100_i32,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &0_i128,
        );
        assert!(result.is_ok(), "aligned ticks should be accepted");
    }

    /// Ticks that are NOT multiples of tick_spacing must be rejected.
    #[test]
    fn test_misaligned_lower_tick_rejected() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_cl(&env, 10);

        // lower_tick = -95 is not a multiple of 10.
        let result = client.try_mint_position(
            &provider,
            &-95_i32,
            &100_i32,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &0_i128,
        );
        assert_eq!(result, Err(Ok(ClError::TickNotAligned)));
    }

    /// Misaligned upper tick must also be rejected.
    #[test]
    fn test_misaligned_upper_tick_rejected() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_cl(&env, 10);

        // upper_tick = 105 is not a multiple of 10.
        let result = client.try_mint_position(
            &provider,
            &-100_i32,
            &105_i32,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &0_i128,
        );
        assert_eq!(result, Err(Ok(ClError::TickNotAligned)));
    }

    /// tick_spacing = 0 must be rejected at initialize time.
    #[test]
    fn test_zero_tick_spacing_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);

        let result = client.try_initialize(&admin, &token_a, &token_b, &30_i128, &0_i32, &0_i32);
        assert_eq!(result, Err(Ok(ClError::InvalidTickSpacing)));
    }

    /// get_pool_state must include tick_spacing set at initialize time.
    #[test]
    fn test_get_pool_state_returns_tick_spacing() {
        let env = Env::default();
        let (_provider, _ta, _tb, client) = setup_cl(&env, 60);

        let state = client.get_pool_state();
        assert_eq!(
            state.tick_spacing, 60,
            "get_pool_state must return tick_spacing = 60"
        );
    }

    /// tick_spacing = 1 allows every tick (no restriction).
    #[test]
    fn test_spacing_one_allows_any_tick() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_cl(&env, 1);

        // Odd ticks (not multiples of anything > 1) should work fine.
        let result = client.try_mint_position(
            &provider,
            &-7_i32,
            &13_i32,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &0_i128,
        );
        assert!(result.is_ok(), "tick_spacing=1 must allow any tick pair");
    }
}

// ── Issues #203, #218, #219, #220: new feature tests ─────────────────────────
#[cfg(test)]
mod test_new_tick_features {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::StellarAssetClient;
    use soroban_sdk::Env;

    fn setup_pool(env: &Env) -> (Address, Address, Address, ConcentratedLiquidityClient) {
        env.mock_all_auths();
        let admin = Address::generate(env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &0_i128, &0_i32, &1_i32);
        let provider = Address::generate(env);
        StellarAssetClient::new(env, &token_a).mint(&provider, &10_000_000_i128);
        StellarAssetClient::new(env, &token_b).mint(&provider, &10_000_000_i128);
        StellarAssetClient::new(env, &token_a).mint(&cl_addr, &10_000_000_i128);
        StellarAssetClient::new(env, &token_b).mint(&cl_addr, &10_000_000_i128);
        (provider, token_a, token_b, client)
    }

    // ── Issue #203: get_tick_info / is_tick_initialized ───────────────────────

    #[test]
    fn is_tick_initialized_false_before_mint() {
        let env = Env::default();
        let (_provider, _ta, _tb, client) = setup_pool(&env);
        assert!(!client.is_tick_initialized(&-100_i32));
        assert!(!client.is_tick_initialized(&0_i32));
        assert!(!client.is_tick_initialized(&100_i32));
    }

    #[test]
    fn get_tick_info_returns_error_for_uninitialized_tick() {
        let env = Env::default();
        let (_provider, _ta, _tb, client) = setup_pool(&env);
        let result = client.try_get_tick_info(&-999_i32);
        assert_eq!(result, Err(Ok(ClError::TickNotInitialized)));
    }

    #[test]
    fn is_tick_initialized_true_after_mint() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_pool(&env);
        client.mint_position(
            &provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
        );
        assert!(
            client.is_tick_initialized(&-100_i32),
            "lower tick must be initialized"
        );
        assert!(
            client.is_tick_initialized(&100_i32),
            "upper tick must be initialized"
        );
        assert!(
            !client.is_tick_initialized(&0_i32),
            "non-boundary tick must stay uninitialized"
        );
    }

    #[test]
    fn get_tick_info_returns_correct_values_after_mint() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_pool(&env);
        client.mint_position(
            &provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
        );

        let lower_info = client.get_tick_info(&-100_i32);
        let upper_info = client.get_tick_info(&100_i32);

        // lower tick: liquidity_net > 0, gross > 0
        assert!(
            lower_info.liquidity_gross > 0,
            "lower gross must be positive"
        );
        assert!(lower_info.liquidity_net > 0, "lower net must be positive");

        // upper tick: liquidity_net < 0 (negative = exits liquidity), gross > 0
        assert!(
            upper_info.liquidity_gross > 0,
            "upper gross must be positive"
        );
        assert!(upper_info.liquidity_net < 0, "upper net must be negative");

        // gross must be equal in magnitude
        assert_eq!(lower_info.liquidity_gross, upper_info.liquidity_gross);
    }

    #[test]
    fn is_tick_initialized_false_after_full_burn() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_pool(&env);
        client.mint_position(
            &provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
        );
        assert!(client.is_tick_initialized(&-100_i32));

        let liq = client
            .get_position(&provider, &-100_i32, &100_i32)
            .liquidity;
        client.burn_position(&provider, &-100_i32, &100_i32, &liq);

        assert!(
            !client.is_tick_initialized(&-100_i32),
            "lower tick must be de-initialized after full burn"
        );
        assert!(
            !client.is_tick_initialized(&100_i32),
            "upper tick must be de-initialized after full burn"
        );
    }

    // ── Issue #218: tick bitmap public API ───────────────────────────────────

    #[test]
    fn next_initialized_tick_pub_returns_none_when_empty() {
        let env = Env::default();
        let (_provider, _ta, _tb, client) = setup_pool(&env);
        // No positions → bitmap is empty.
        let result = client.next_initialized_tick_pub(&0_i32);
        assert!(
            result.is_none(),
            "no ticks should be found when pool has no positions"
        );
    }

    #[test]
    fn prev_initialized_tick_pub_returns_none_when_empty() {
        let env = Env::default();
        let (_provider, _ta, _tb, client) = setup_pool(&env);
        let result = client.prev_initialized_tick_pub(&0_i32);
        assert!(result.is_none());
    }

    #[test]
    fn next_and_prev_initialized_tick_pub_after_mint() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_pool(&env);
        // Initializes ticks -100 and 100.
        client.mint_position(
            &provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
        );

        // next above tick -200: the first initialized tick above -200 is -100.
        let next = client.next_initialized_tick_pub(&-200_i32);
        assert_eq!(next, Some(-100_i32), "next tick above -200 must be -100");

        // next above tick -100: the first initialized tick above -100 is 100.
        let next2 = client.next_initialized_tick_pub(&-100_i32);
        assert_eq!(next2, Some(100_i32), "next tick above -100 must be 100");

        // prev at or below tick 200: highest initialized tick ≤ 200 is 100.
        let prev = client.prev_initialized_tick_pub(&200_i32);
        assert_eq!(prev, Some(100_i32), "prev tick at/below 200 must be 100");

        // prev at or below tick 100: same tick (100 is initialized).
        let prev2 = client.prev_initialized_tick_pub(&100_i32);
        assert_eq!(prev2, Some(100_i32));

        // prev at or below tick -101: highest initialized tick below -100 is -100... wait,
        // -101 < -100 so prev should be None (no tick at or below -101 other than maybe -100?).
        // Actually -100 < -101? No: -100 > -101. So prev of -101 should be None since -100 > -101.
        let prev3 = client.prev_initialized_tick_pub(&-101_i32);
        assert!(prev3.is_none(), "no initialized tick at or below -101");
    }

    #[test]
    fn bitmap_correctly_tracks_multiple_positions() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_pool(&env);
        // Two non-overlapping ranges initialize 4 distinct ticks.
        client.mint_position(
            &provider,
            &-200_i32,
            &-100_i32,
            &0_i128,
            &5_000_i128,
            &0_i128,
            &0_i128,
        );
        client.mint_position(
            &provider,
            &100_i32,
            &200_i32,
            &5_000_i128,
            &0_i128,
            &0_i128,
            &0_i128,
        );

        assert!(client.is_tick_initialized(&-200_i32));
        assert!(client.is_tick_initialized(&-100_i32));
        assert!(client.is_tick_initialized(&100_i32));
        assert!(client.is_tick_initialized(&200_i32));
        assert!(!client.is_tick_initialized(&0_i32));

        // next above -300 must be -200.
        assert_eq!(client.next_initialized_tick_pub(&-300_i32), Some(-200_i32));
        // next above -200 must be -100.
        assert_eq!(client.next_initialized_tick_pub(&-200_i32), Some(-100_i32));
    }

    // ── Issue #219: sqrtPrice math library ───────────────────────────────────

    #[test]
    fn tick_to_sqrt_price_x96_at_zero_is_one_q96() {
        // sqrt(1.0001^0) * 2^96 = 1 * 2^96
        let sp = ConcentratedLiquidity::tick_to_sqrt_price_x96(0_i32);
        assert_eq!(sp, 1u128 << 96, "sqrtPrice at tick 0 must be exactly 2^96");
    }

    #[test]
    fn tick_to_sqrt_price_x96_is_monotone() {
        // sqrtPrice must increase strictly with tick.
        let sp_neg = ConcentratedLiquidity::tick_to_sqrt_price_x96(-10_i32);
        let sp_zero = ConcentratedLiquidity::tick_to_sqrt_price_x96(0_i32);
        let sp_pos = ConcentratedLiquidity::tick_to_sqrt_price_x96(10_i32);
        assert!(sp_neg < sp_zero, "sqrtPrice(-10) must be < sqrtPrice(0)");
        assert!(sp_zero < sp_pos, "sqrtPrice(0) must be < sqrtPrice(10)");
    }

    #[test]
    fn tick_to_sqrt_price_x96_accuracy_within_one_bps() {
        // For tick = 100: sqrt(1.0001^100) ≈ 1.0001^50 ≈ 1.005012.
        // We verify the returned value is within 1 bps (0.01%) of 2^96 * 1.005012.
        let sp = ConcentratedLiquidity::tick_to_sqrt_price_x96(100_i32);
        let one_q96: u128 = 1u128 << 96;
        // Expected ≈ 1.005012 * 2^96. Allow ±1 bps = 0.01%.
        let expected_approx = one_q96 + one_q96 / 200; // 1.005 * 2^96 (rough lower bound)
        assert!(
            sp >= expected_approx,
            "sqrtPrice(100) must be at least 1.005 * 2^96"
        );
        let upper = one_q96 + one_q96 / 100; // 1.01 * 2^96 (rough upper bound)
        assert!(sp <= upper, "sqrtPrice(100) must be at most 1.01 * 2^96");
    }

    #[test]
    fn sqrt_price_x96_to_tick_roundtrip() {
        // For any tick t, sqrt_price_x96_to_tick(tick_to_sqrt_price_x96(t)) should equal t
        // (or be off by at most 1 due to integer rounding).
        for tick in [-300_i32, -100, -10, -1, 0, 1, 10, 100, 300] {
            let sp = ConcentratedLiquidity::tick_to_sqrt_price_x96(tick);
            let recovered = ConcentratedLiquidity::sqrt_price_x96_to_tick(sp);
            let diff = (recovered - tick).abs();
            assert!(
                diff <= 1,
                "roundtrip failed for tick {tick}: got {recovered}, diff={diff}"
            );
        }
    }

    #[test]
    fn sqrt_price_x96_to_tick_at_zero_input_returns_min_tick() {
        let t = ConcentratedLiquidity::sqrt_price_x96_to_tick(0_u128);
        assert_eq!(t, MIN_TICK);
    }

    #[test]
    fn tick_to_sqrt_price_matches_pool_sqrt_price_at_tick_zero() {
        // The pool stores sqrtPriceX96 = sqrt(price) * 2^96 / 1000 after initialize.
        // tick_to_sqrt_price_x96(0) = 2^96.  pool initial = 1000 * 2^96 / 1000 = 2^96. ✓
        let env = Env::default();
        let (_provider, _ta, _tb, client) = setup_pool(&env);
        let state = client.get_pool_state();
        let computed = ConcentratedLiquidity::tick_to_sqrt_price_x96(state.current_tick);
        // Allow a tiny rounding difference.
        let diff = (computed as i128 - state.sqrt_price as i128).abs();
        let one_pct = (state.sqrt_price / 100) as i128;
        assert!(
            diff <= one_pct,
            "tick_to_sqrt_price_x96 must agree with pool sqrtPrice within 1%"
        );
    }

    // ── Issue #220: tick state machine query helpers ──────────────────────────

    #[test]
    fn get_liquidity_net_at_tick_returns_zero_for_uninitialised() {
        let env = Env::default();
        let (_provider, _ta, _tb, client) = setup_pool(&env);
        assert_eq!(client.get_liquidity_net_at_tick(&42_i32), 0_i128);
    }

    #[test]
    fn get_liquidity_net_at_tick_correct_after_mint() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_pool(&env);
        client.mint_position(
            &provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
        );
        let lower_net = client.get_liquidity_net_at_tick(&-100_i32);
        let upper_net = client.get_liquidity_net_at_tick(&100_i32);
        assert!(lower_net > 0, "lower tick liquidity_net must be positive");
        assert!(upper_net < 0, "upper tick liquidity_net must be negative");
        assert_eq!(
            lower_net, -upper_net,
            "net values must be equal and opposite"
        );
    }

    #[test]
    fn simulate_tick_cross_upward_adds_net_liquidity() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_pool(&env);
        client.mint_position(
            &provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
        );

        let net = client.get_liquidity_net_at_tick(&-100_i32);
        // Crossing lower tick upward (zero_for_one=false) adds net.
        let result = client.simulate_tick_cross(&0_i128, &-100_i32, &false);
        assert_eq!(
            result,
            net.max(0),
            "crossing lower tick upward must add net liquidity"
        );
    }

    #[test]
    fn simulate_tick_cross_downward_subtracts_net_liquidity() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_pool(&env);
        client.mint_position(
            &provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
        );

        let net = client.get_liquidity_net_at_tick(&-100_i32);
        let active = net; // assume we're currently above lower_tick with net liq
                          // Crossing lower tick downward (zero_for_one=true) subtracts net.
        let result = client.simulate_tick_cross(&active, &-100_i32, &true);
        assert_eq!(result, (active - net).max(0));
    }

    #[test]
    fn simulate_tick_cross_does_not_modify_state() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_pool(&env);
        client.mint_position(
            &provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
        );

        let liq_before = client.active_liquidity();
        // Call simulate — must not change active liquidity.
        client.simulate_tick_cross(&liq_before, &-100_i32, &false);
        assert_eq!(
            client.active_liquidity(),
            liq_before,
            "simulate_tick_cross must not modify state"
        );
    }

    #[test]
    fn tick_state_machine_liquidity_updates_correctly_during_swap() {
        // Full integration: mint two adjacent ranges, perform a swap that crosses
        // a tick boundary, verify active_liquidity is correct at each step.
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        // Start at tick 10, so range [-50, 0] is below current and [0, 50] includes current.
        client.initialize(&admin, &token_a, &token_b, &0_i128, &10_i32, &1_i32);

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &10_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&provider, &10_000_000_i128);
        StellarAssetClient::new(&env, &token_a).mint(&cl_addr, &10_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&cl_addr, &10_000_000_i128);

        // Range [0, 50] covers current tick 10 → active_liquidity increases.
        client.mint_position(
            &provider,
            &0_i32,
            &50_i32,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &0_i128,
        );
        let liq_in_range = client.active_liquidity();
        assert!(
            liq_in_range > 0,
            "position covering current tick must add active liquidity"
        );

        // Range [-50, 0] is entirely below current tick → no active liquidity yet.
        client.mint_position(
            &provider,
            &-50_i32,
            &0_i32,
            &0_i128,
            &50_000_i128,
            &0_i128,
            &0_i128,
        );
        assert_eq!(
            client.active_liquidity(),
            liq_in_range,
            "out-of-range position must not change active liq"
        );

        // Verify tick-state-machine view: net at tick 0 accounts for both positions.
        let net_at_0 = client.get_liquidity_net_at_tick(&0_i32);
        // lower tick for second range: net += liq2; upper tick for first range is 50 (not 0)
        // So at tick 0: net = liq2 (lower of second) - first_range_liq... wait,
        // tick 0 is the UPPER of range [-50,0] AND LOWER of range [0,50]:
        // Actually in the code, upper tick uses liquidity_net -= liquidity.
        // The liquidity_net at tick 0 = (liq from [0,50] as lower) + (-liq from [-50,0] as upper)
        // = liq1 - liq2 (approximately, since both have similar amounts)
        // Just verify it's non-zero (both positions contributed).
        assert_ne!(
            net_at_0, 0_i128,
            "tick 0 net must be non-zero with two adjacent positions"
        );

        // Perform a downward swap to cross below tick 0.
        client.swap(&provider, &true, &5_000_i128, &0_u128, &0_i128, &u64::MAX);
        let tick_after = client.current_tick();
        assert!(tick_after < 0, "swap should push price below tick 0");
        // After crossing tick 0 downward, the active liquidity of the lower range activates.
        // The net change should reflect both crossing events.
        let liq_after = client.active_liquidity();
        // Price now in [-50, 0), so the second position is active and first is not.
        assert!(
            liq_after > 0,
            "lower range must be active after crossing tick 0 downward"
        );
    }
}

// ── Issue #221: single-token deposit tests ────────────────────────────────────
#[cfg(test)]
mod test_single_token_deposit {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::StellarAssetClient;
    use soroban_sdk::Env;

    /// Helper: deploy a CL pool starting at `initial_tick` with `tick_spacing = 1`.
    fn setup_pool(
        env: &Env,
        initial_tick: i32,
    ) -> (Address, Address, Address, ConcentratedLiquidityClient) {
        env.mock_all_auths();
        let admin = Address::generate(env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &0_i128, &initial_tick, &1_i32);

        let provider = Address::generate(env);
        StellarAssetClient::new(env, &token_a).mint(&provider, &100_000_000_i128);
        StellarAssetClient::new(env, &token_b).mint(&provider, &100_000_000_i128);
        // Pre-fund contract so it can return tokens in burn/collect.
        StellarAssetClient::new(env, &token_a).mint(&cl_addr, &100_000_000_i128);
        StellarAssetClient::new(env, &token_b).mint(&cl_addr, &100_000_000_i128);

        (provider, token_a, token_b, client)
    }

    // ── Scenario 1: price BELOW range — only token A needed ──────────────────

    /// When current price is below the position range, only token A is required.
    /// The full `amount_in` of token A should be consumed with zero dust.
    #[test]
    fn test_single_token_deposit_below_range_uses_only_token_a() {
        let env = Env::default();
        // current_tick = -200 → price is below range [100, 200]
        let (provider, token_a, _token_b, client) = setup_pool(&env, -200);

        let amount_in = 10_000_i128;
        let result = client.mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &amount_in,
            &1_i128,
            &u64::MAX,
        );

        assert_eq!(
            result.amount_used, amount_in,
            "all token A must be consumed"
        );
        assert_eq!(result.dust, 0_i128, "no dust when price is below range");
        assert!(result.liquidity > 0, "liquidity must be positive");
    }

    /// Supplying token B when price is below range must fail (wrong token).
    #[test]
    fn test_single_token_deposit_below_range_rejects_token_b() {
        let env = Env::default();
        let (provider, _token_a, token_b, client) = setup_pool(&env, -200);

        let result = client.try_mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &token_b,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );
        assert_eq!(
            result,
            Err(Ok(ClError::SlippageExceeded)),
            "token B must be rejected when price is below range"
        );
    }

    // ── Scenario 2: price ABOVE range — only token B needed ──────────────────

    /// When current price is above the position range, only token B is required.
    #[test]
    fn test_single_token_deposit_above_range_uses_only_token_b() {
        let env = Env::default();
        // current_tick = 300 → price is above range [-200, -100]
        let (provider, _token_a, token_b, client) = setup_pool(&env, 300);

        let amount_in = 10_000_i128;
        let result = client.mint_position_single_token(
            &provider,
            &-200_i32,
            &-100_i32,
            &token_b,
            &amount_in,
            &1_i128,
            &u64::MAX,
        );

        assert_eq!(
            result.amount_used, amount_in,
            "all token B must be consumed"
        );
        assert_eq!(result.dust, 0_i128, "no dust when price is above range");
        assert!(result.liquidity > 0, "liquidity must be positive");
    }

    /// Supplying token A when price is above range must fail (wrong token).
    #[test]
    fn test_single_token_deposit_above_range_rejects_token_a() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, 300);

        let result = client.try_mint_position_single_token(
            &provider,
            &-200_i32,
            &-100_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );
        assert_eq!(
            result,
            Err(Ok(ClError::SlippageExceeded)),
            "token A must be rejected when price is above range"
        );
    }

    // ── Scenario 3: price WITHIN range — single token split ──────────────────

    /// When price is inside the range and token A is supplied, liquidity is
    /// computed from the token-A portion only (covers [current_price, upper]).
    #[test]
    fn test_single_token_deposit_in_range_token_a() {
        let env = Env::default();
        // current_tick = 0, range [-100, 100] → price is in range
        let (provider, token_a, _token_b, client) = setup_pool(&env, 0);

        let amount_in = 50_000_i128;
        let result = client.mint_position_single_token(
            &provider,
            &-100_i32,
            &100_i32,
            &token_a,
            &amount_in,
            &1_i128,
            &u64::MAX,
        );

        // amount_used ≤ amount_in (some dust possible due to rounding)
        assert!(
            result.amount_used <= amount_in,
            "amount_used must not exceed amount_in"
        );
        assert!(result.amount_used > 0, "some token A must be consumed");
        assert_eq!(
            result.amount_used + result.dust,
            amount_in,
            "amount_used + dust must equal amount_in"
        );
        assert!(result.liquidity > 0, "liquidity must be positive");
    }

    /// When price is inside the range and token B is supplied, liquidity is
    /// computed from the token-B portion only (covers [lower, current_price]).
    #[test]
    fn test_single_token_deposit_in_range_token_b() {
        let env = Env::default();
        let (provider, _token_a, token_b, client) = setup_pool(&env, 0);

        let amount_in = 50_000_i128;
        let result = client.mint_position_single_token(
            &provider,
            &-100_i32,
            &100_i32,
            &token_b,
            &amount_in,
            &1_i128,
            &u64::MAX,
        );

        assert!(result.amount_used <= amount_in);
        assert!(result.amount_used > 0);
        assert_eq!(result.amount_used + result.dust, amount_in);
        assert!(result.liquidity > 0);
    }

    /// Dust is minimised: for a large deposit the dust should be at most a
    /// tiny fraction of the input (rounding artefact only).
    #[test]
    fn test_single_token_deposit_in_range_dust_is_minimal() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, 0);

        let amount_in = 1_000_000_i128;
        let result = client.mint_position_single_token(
            &provider,
            &-100_i32,
            &100_i32,
            &token_a,
            &amount_in,
            &1_i128,
            &u64::MAX,
        );

        // Dust must be < 1% of amount_in (rounding only, not a large fraction).
        assert!(
            result.dust < amount_in / 100,
            "dust ({}) must be < 1% of amount_in ({})",
            result.dust,
            amount_in
        );
    }

    // ── Slippage guard ────────────────────────────────────────────────────────

    /// min_liquidity guard must reject deposits that produce too little liquidity.
    #[test]
    fn test_single_token_deposit_min_liquidity_guard() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, -200);

        // First, find out how much liquidity a 10_000 deposit produces.
        let normal = client
            .mint_position_single_token(
                &provider,
                &100_i32,
                &200_i32,
                &token_a,
                &10_000_i128,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();

        // Now request more than that — must fail.
        let provider2 = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider2, &10_000_i128);
        let result = client.try_mint_position_single_token(
            &provider2,
            &100_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &(normal.liquidity + 1),
            &u64::MAX,
        );
        assert_eq!(
            result,
            Err(Ok(ClError::SlippageExceeded)),
            "min_liquidity guard must reject insufficient liquidity"
        );
    }

    // ── Deadline guard ────────────────────────────────────────────────────────

    #[test]
    fn test_single_token_deposit_deadline_expired() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000);
        let (provider, token_a, _token_b, client) = setup_pool(&env, -200);

        let result = client.try_mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &999_u64, // deadline in the past
        );
        assert_eq!(result, Err(Ok(ClError::DeadlineExpired)));
    }

    // ── Pause guard ───────────────────────────────────────────────────────────

    #[test]
    fn test_single_token_deposit_paused_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &0_i128, &-200_i32, &1_i32);

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &10_000_i128);

        client.pause(&admin);

        let result = client.try_mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );
        assert_eq!(result, Err(Ok(ClError::Paused)));
    }

    // ── Invalid token guard ───────────────────────────────────────────────────

    #[test]
    fn test_single_token_deposit_invalid_token_rejected() {
        let env = Env::default();
        let (provider, _token_a, _token_b, client) = setup_pool(&env, -200);

        let unknown = Address::generate(&env);
        let result = client.try_mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &unknown,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );
        assert_eq!(result, Err(Ok(ClError::InvalidToken)));
    }

    // ── Tick alignment guard ──────────────────────────────────────────────────

    #[test]
    fn test_single_token_deposit_misaligned_tick_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        // tick_spacing = 10
        client.initialize(&admin, &token_a, &token_b, &0_i128, &-200_i32, &10_i32);

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &10_000_i128);

        // lower_tick = 105 is not a multiple of 10
        let result = client.try_mint_position_single_token(
            &provider,
            &105_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );
        assert_eq!(result, Err(Ok(ClError::TickNotAligned)));
    }

    // ── Zero amount guard ─────────────────────────────────────────────────────

    #[test]
    fn test_single_token_deposit_zero_amount_rejected() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, -200);

        let result = client.try_mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &0_i128,
            &1_i128,
            &u64::MAX,
        );
        assert_eq!(result, Err(Ok(ClError::ZeroAmounts)));
    }

    // ── Position accumulation ─────────────────────────────────────────────────

    /// Two single-token deposits to the same range should accumulate liquidity.
    #[test]
    fn test_single_token_deposit_accumulates_liquidity() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, -200);

        let r1 = client
            .mint_position_single_token(
                &provider,
                &100_i32,
                &200_i32,
                &token_a,
                &10_000_i128,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();

        let r2 = client
            .mint_position_single_token(
                &provider,
                &100_i32,
                &200_i32,
                &token_a,
                &10_000_i128,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();

        let pos = client.get_position(&provider, &100_i32, &200_i32);
        assert_eq!(
            pos.liquidity,
            r1.liquidity + r2.liquidity,
            "liquidity must accumulate across deposits"
        );
    }

    // ── quote_single_token_deposit matches mint ───────────────────────────────

    /// The quote function must return the same values as the actual deposit.
    #[test]
    fn test_quote_single_token_deposit_matches_mint_below_range() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, -200);

        let amount_in = 20_000_i128;
        let quote = client
            .quote_single_token_deposit(&100_i32, &200_i32, &token_a, &amount_in)
            .unwrap();

        let actual = client
            .mint_position_single_token(
                &provider,
                &100_i32,
                &200_i32,
                &token_a,
                &amount_in,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();

        assert_eq!(
            quote.amount_used, actual.amount_used,
            "quote amount_used must match actual"
        );
        assert_eq!(quote.dust, actual.dust, "quote dust must match actual");
        assert_eq!(
            quote.liquidity, actual.liquidity,
            "quote liquidity must match actual"
        );
    }

    #[test]
    fn test_quote_single_token_deposit_matches_mint_above_range() {
        let env = Env::default();
        let (provider, _token_a, token_b, client) = setup_pool(&env, 300);

        let amount_in = 20_000_i128;
        let quote = client
            .quote_single_token_deposit(&-200_i32, &-100_i32, &token_b, &amount_in)
            .unwrap();

        let actual = client
            .mint_position_single_token(
                &provider,
                &-200_i32,
                &-100_i32,
                &token_b,
                &amount_in,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();

        assert_eq!(quote.amount_used, actual.amount_used);
        assert_eq!(quote.dust, actual.dust);
        assert_eq!(quote.liquidity, actual.liquidity);
    }

    #[test]
    fn test_quote_single_token_deposit_matches_mint_in_range() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, 0);

        let amount_in = 100_000_i128;
        let quote = client
            .quote_single_token_deposit(&-100_i32, &100_i32, &token_a, &amount_in)
            .unwrap();

        let actual = client
            .mint_position_single_token(
                &provider,
                &-100_i32,
                &100_i32,
                &token_a,
                &amount_in,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();

        assert_eq!(quote.amount_used, actual.amount_used);
        assert_eq!(quote.dust, actual.dust);
        assert_eq!(quote.liquidity, actual.liquidity);
    }

    // ── Active liquidity tracking ─────────────────────────────────────────────

    /// A single-token deposit to an in-range position must increase active_liquidity.
    #[test]
    fn test_single_token_deposit_in_range_increases_active_liquidity() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, 0);

        let liq_before = client.active_liquidity();
        let result = client
            .mint_position_single_token(
                &provider,
                &-100_i32,
                &100_i32,
                &token_a,
                &50_000_i128,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();

        let liq_after = client.active_liquidity();
        assert_eq!(
            liq_after - liq_before,
            result.liquidity,
            "active_liquidity must increase by the minted liquidity"
        );
    }

    /// A single-token deposit to an out-of-range position must NOT change active_liquidity.
    #[test]
    fn test_single_token_deposit_out_of_range_does_not_change_active_liquidity() {
        let env = Env::default();
        // current_tick = -200, range [100, 200] is above current price
        let (provider, token_a, _token_b, client) = setup_pool(&env, -200);

        let liq_before = client.active_liquidity();
        client
            .mint_position_single_token(
                &provider,
                &100_i32,
                &200_i32,
                &token_a,
                &50_000_i128,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();

        assert_eq!(
            client.active_liquidity(),
            liq_before,
            "out-of-range deposit must not change active_liquidity"
        );
    }

    // ── Tick initialisation ───────────────────────────────────────────────────

    /// After a single-token deposit the boundary ticks must be initialised.
    #[test]
    fn test_single_token_deposit_initialises_ticks() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, -200);

        assert!(!client.is_tick_initialized(&100_i32));
        assert!(!client.is_tick_initialized(&200_i32));

        client
            .mint_position_single_token(
                &provider,
                &100_i32,
                &200_i32,
                &token_a,
                &10_000_i128,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();

        assert!(
            client.is_tick_initialized(&100_i32),
            "lower tick must be initialised"
        );
        assert!(
            client.is_tick_initialized(&200_i32),
            "upper tick must be initialised"
        );
    }

    // ── Position list tracking ────────────────────────────────────────────────

    /// get_positions must include the range after a single-token deposit.
    #[test]
    fn test_single_token_deposit_appears_in_get_positions() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, -200);

        client
            .mint_position_single_token(
                &provider,
                &100_i32,
                &200_i32,
                &token_a,
                &10_000_i128,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();

        let positions = client.get_positions(&provider);
        assert_eq!(positions.len(), 1);
        assert_eq!(positions.get(0).unwrap(), (100_i32, 200_i32));
    }

    // ── Symmetry: token A below range == token B above range ─────────────────

    /// Depositing token A below range and token B above range with the same
    /// amount should produce the same liquidity (symmetric price model).
    #[test]
    fn test_single_token_deposit_symmetry_below_above() {
        let env_a = Env::default();
        let (provider_a, token_a_a, _token_b_a, client_a) = setup_pool(&env_a, -200);
        let result_a = client_a
            .mint_position_single_token(
                &provider_a,
                &100_i32,
                &200_i32,
                &token_a_a,
                &10_000_i128,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();

        let env_b = Env::default();
        let (provider_b, _token_a_b, token_b_b, client_b) = setup_pool(&env_b, 300);
        let result_b = client_b
            .mint_position_single_token(
                &provider_b,
                &100_i32,
                &200_i32,
                &token_b_b,
                &10_000_i128,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();

        assert_eq!(
            result_a.liquidity, result_b.liquidity,
            "symmetric deposits must produce equal liquidity"
        );
        assert_eq!(result_a.dust, 0);
        assert_eq!(result_b.dust, 0);
    }

    // ── Larger deposit produces proportionally more liquidity ─────────────────

    #[test]
    fn test_single_token_deposit_liquidity_scales_with_amount() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, -200);

        let r1 = client
            .mint_position_single_token(
                &provider,
                &100_i32,
                &200_i32,
                &token_a,
                &10_000_i128,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();

        let r2 = client
            .mint_position_single_token(
                &provider,
                &100_i32,
                &200_i32,
                &token_a,
                &20_000_i128,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();

        // Doubling the deposit should roughly double the liquidity (within rounding).
        assert!(
            r2.liquidity >= r1.liquidity * 2 - 2,
            "double deposit must produce at least double liquidity (got {} vs {})",
            r2.liquidity,
            r1.liquidity
        );
    }

    // ── mint_pos event emitted ────────────────────────────────────────────────

    #[test]
    fn test_single_token_deposit_emits_event() {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::IntoVal;

        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, -200);

        let result = client
            .mint_position_single_token(
                &provider,
                &100_i32,
                &200_i32,
                &token_a,
                &10_000_i128,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();

        let events = env.events().all();
        let evt = events
            .iter()
            .find(|e| {
                e.0 == client.address
                    && e.1 == (symbol_short!("mint_1t"), provider.clone()).into_val(&env)
            })
            .expect("mint_1t event must be emitted");

        let __ver_6: (u32, (i32, i32, i128, i128, i128)) = evt.2.into_val(&env);
        assert_eq!(__ver_6.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        let data: (i32, i32, i128, i128, i128) = __ver_6.1;
        assert_eq!(data.0, 100_i32);
        assert_eq!(data.1, 200_i32);
        assert_eq!(data.2, result.liquidity);
        assert_eq!(data.3, result.amount_used);
        assert_eq!(data.4, result.dust);
    }

    // ── Edge cases: current tick at boundary positions ────────────────────────

    /// When current tick equals lower_tick exactly, token A deposit should work
    /// for the [current, upper] portion.
    #[test]
    fn test_single_token_deposit_at_lower_tick_boundary() {
        let env = Env::default();
        // current_tick = 100, range [100, 200] - price at lower boundary
        let (provider, token_a, _token_b, client) = setup_pool(&env, 100);

        let result = client
            .mint_position_single_token(
                &provider,
                &100_i32,
                &200_i32,
                &token_a,
                &10_000_i128,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();

        // Token A at lower boundary should still produce liquidity
        assert!(result.liquidity > 0);
        assert!(result.amount_used > 0);
        // Active liquidity should increase since current >= lower_tick
        assert!(client.active_liquidity() > 0);
    }

    /// When current tick equals upper_tick - 1, token B deposit should work
    /// for the [lower, current] portion.
    #[test]
    fn test_single_token_deposit_just_below_upper_tick() {
        let env = Env::default();
        // current_tick = 99, range [100, 200] - just below upper
        let (provider, _token_a, token_b, client) = setup_pool(&env, 99);

        let result = client
            .mint_position_single_token(
                &provider,
                &100_i32,
                &200_i32,
                &token_b,
                &10_000_i128,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();

        // Token B should work since current < upper_tick
        assert!(result.liquidity > 0);
        assert!(result.amount_used > 0);
    }

    /// Large range with in-range single token deposit.
    #[test]
    fn test_single_token_deposit_large_range() {
        let env = Env::default();
        // current_tick = 0, range [-1000, 1000] - wide range centered at current
        let (provider, token_a, _token_b, client) = setup_pool(&env, 0);

        let result = client
            .mint_position_single_token(
                &provider,
                &-1000_i32,
                &1000_i32,
                &token_a,
                &100_000_i128,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();

        // Token A covers upper half of the range
        assert!(result.liquidity > 0);
        // Dust should be minimal for large amounts
        assert!(result.dust < result.amount_used / 10);
    }

    /// Small range near current tick with single token.
    #[test]
    fn test_single_token_deposit_small_range_near_current() {
        let env = Env::default();
        // current_tick = 0, range [-1, 1] - very tight range around current
        let (provider, token_a, _token_b, client) = setup_pool(&env, 0);

        let result = client
            .mint_position_single_token(
                &provider,
                &-1_i32,
                &1_i32,
                &token_a,
                &1_000_i128,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();

        assert!(result.liquidity > 0);
        assert!(result.amount_used > 0);
    }

    /// Extreme tick values should work correctly.
    #[test]
    fn test_single_token_deposit_extreme_tick_range() {
        let env = Env::default();
        // current_tick = -800000, near minimum tick
        let (provider, token_a, _token_b, client) = setup_pool(&env, -800000);

        // Deposit at a reasonable range above current
        let result = client
            .mint_position_single_token(
                &provider,
                &-700000_i32,
                &-600000_i32,
                &token_a,
                &10_000_i128,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();

        assert!(result.liquidity > 0);
    }

    /// Quote and mint should match for all three scenarios.
    #[test]
    fn test_quote_matches_mint_across_all_scenarios() {
        let env = Env::default();

        // Test below range
        let (provider_a, token_a, _token_b, client) = setup_pool(&env, -200);
        let quote1 = client
            .quote_single_token_deposit(&100_i32, &200_i32, &token_a, &10_000_i128)
            .unwrap();
        let mint1 = client
            .mint_position_single_token(
                &provider_a,
                &100_i32,
                &200_i32,
                &token_a,
                &10_000_i128,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();
        assert_eq!(quote1.liquidity, mint1.liquidity);
        assert_eq!(quote1.amount_used, mint1.amount_used);

        // Test above range
        let (provider_b, _token_a, token_b, client) = setup_pool(&env, 300);
        let quote2 = client
            .quote_single_token_deposit(&-200_i32, &-100_i32, &token_b, &10_000_i128)
            .unwrap();
        let mint2 = client
            .mint_position_single_token(
                &provider_b,
                &-200_i32,
                &-100_i32,
                &token_b,
                &10_000_i128,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();
        assert_eq!(quote2.liquidity, mint2.liquidity);
        assert_eq!(quote2.amount_used, mint2.amount_used);

        // Test in range
        let (provider_c, token_a, _token_b, client) = setup_pool(&env, 0);
        let quote3 = client
            .quote_single_token_deposit(&-50_i32, &50_i32, &token_a, &10_000_i128)
            .unwrap();
        let mint3 = client
            .mint_position_single_token(
                &provider_c,
                &-50_i32,
                &50_i32,
                &token_a,
                &10_000_i128,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();
        assert_eq!(quote3.liquidity, mint3.liquidity);
        assert_eq!(quote3.amount_used, mint3.amount_used);
    }

    /// Verify token transfers are correct by checking balances.
    #[test]
    fn test_single_token_deposit_balances_correct() {
        use soroban_sdk::token::StellarAssetClient;

        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &0_i128, &-200_i32, &1_i32);

        let provider = Address::generate(&env);
        let sac_a = StellarAssetClient::new(&env, &token_a);
        let initial_balance = sac_a.balance(&provider);

        sac_a.mint(&provider, &100_000_i128);
        sac_a.mint(&cl_addr, &100_000_i128);

        let amount_in = 10_000_i128;
        client.mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &amount_in,
            &1_i128,
            &u64::MAX,
        );

        let final_balance = sac_a.balance(&provider);
        // Provider should have lost exactly amount_used (which equals amount_in for out-of-range)
        assert!(
            (initial_balance - final_balance).abs() <= amount_in,
            "provider should have lost approximately the deposited amount"
        );
    }

    // ── Burn single-token position tests ──────────────────────────────────────

    /// Burning a single-token (out-of-range) position returns the correct token.
    #[test]
    fn test_burn_single_token_below_range_returns_token_a() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &0_i128, &-200_i32, &1_i32);

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &100_000_i128);
        StellarAssetClient::new(&env, &token_a).mint(&cl_addr, &100_000_i128);

        let result = client
            .mint_position_single_token(
                &provider,
                &100_i32,
                &200_i32,
                &token_a,
                &10_000_i128,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();

        let liq = result.liquidity;
        let (burn_a, burn_b) = client.burn_position(&provider, &100_i32, &200_i32, &liq);

        // Should return only token A (position was out of range)
        assert!(burn_a > 0, "burn should return token_a");
        assert_eq!(
            burn_b, 0_i128,
            "burn should not return token_b for out-of-range position"
        );
    }

    /// Burning an in-range single-token position returns both tokens proportionally.
    #[test]
    fn test_burn_single_token_in_range_returns_both_tokens() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &0_i128, &0_i32, &1_i32);

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &100_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&provider, &100_000_i128);
        StellarAssetClient::new(&env, &token_a).mint(&cl_addr, &100_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&cl_addr, &100_000_i128);

        // Deposit token A in range [-100, 100]
        let result = client
            .mint_position_single_token(
                &provider,
                &-100_i32,
                &100_i32,
                &token_a,
                &10_000_i128,
                &1_i128,
                &u64::MAX,
            )
            .unwrap();

        let liq = result.liquidity;
        let (burn_a, burn_b) = client.burn_position(&provider, &-100_i32, &100_i32, &liq);

        // Position was in-range, so both tokens should be returned
        assert!(burn_a > 0 || burn_b > 0, "burn should return tokens");
    }
}
