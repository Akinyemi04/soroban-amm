//! V2-to-V3 atomic migration contract.
//!
//! Steps performed atomically in `migrate`:
//!   1. Pull V2 LP shares from the caller.
//!   2. Burn V2 LP shares to receive token_a and token_b.
//!   3. Compute an optimal CL tick range via `preview_range`.
//!   4. Mint a V3 concentrated-liquidity position with those tokens.
//!   5. Return any dust (tokens not consumed by CL deposit) to the LP.

#![no_std]

use soroban_sdk::{
    contract, contractimpl, contracttype, Address, Env, Symbol,
};
use soroban_sdk::token::Client as TokenClient;
use amm::AmmPoolClient;
use concentrated_liquidity::{ClPoolClient, tick_to_sqrt_price_x96, Q96};

// ── Storage keys ─────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    Admin,
    V2Pool,
    V3Pool,
    V2LpToken,
    TokenA,
    TokenB,
    FeeDiscountBps,
}

// ── Return types ──────────────────────────────────────────────────────────────

#[contracttype]
pub struct MigrateResult {
    pub v3_position_id: i128,
    pub amount_a_deposited: i128,
    pub amount_b_deposited: i128,
    pub dust_a: i128,
    pub dust_b: i128,
    pub lower_tick: i32,
    pub upper_tick: i32,
}

#[contracttype]
pub struct RangeResult {
    pub lower_tick: i32,
    pub upper_tick: i32,
    pub estimated_liquidity: i128,
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct V2ToV3Migration;

#[contractimpl]
impl V2ToV3Migration {
    pub fn initialize(
        env: Env,
        admin: Address,
        v2_pool: Address,
        v3_pool: Address,
        v2_lp_token: Address,
        token_a: Address,
        token_b: Address,
        fee_discount_bps: i128,
    ) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        assert!(fee_discount_bps >= 0 && fee_discount_bps <= 10_000, "invalid discount");

        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::V2Pool, &v2_pool);
        env.storage().instance().set(&DataKey::V3Pool, &v3_pool);
        env.storage().instance().set(&DataKey::V2LpToken, &v2_lp_token);
        env.storage().instance().set(&DataKey::TokenA, &token_a);
        env.storage().instance().set(&DataKey::TokenB, &token_b);
        env.storage().instance().set(&DataKey::FeeDiscountBps, &fee_discount_bps);
    }

    /// Migrate `lp_shares` of V2 liquidity to a V3 concentrated position.
    pub fn migrate(
        env: Env,
        lp: Address,
        lp_shares: i128,
        min_amount_a: i128,
        min_amount_b: i128,
    ) -> MigrateResult {
        lp.require_auth();
        assert!(lp_shares > 0, "lp_shares must be positive");

        let v2_pool_addr: Address = env.storage().instance().get(&DataKey::V2Pool).unwrap();
        let v3_pool_addr: Address = env.storage().instance().get(&DataKey::V3Pool).unwrap();
        let v2_lp_addr: Address = env.storage().instance().get(&DataKey::V2LpToken).unwrap();
        let token_a_addr: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b_addr: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let contract_addr = env.current_contract_address();

        // 1. Pull V2 LP shares from caller
        TokenClient::new(&env, &v2_lp_addr).transfer(&lp, &contract_addr, &lp_shares);

        // 2. Burn V2 LP shares → token_a + token_b
        let v2 = AmmPoolClient::new(&env, &v2_pool_addr);
        let (received_a, received_b) =
            v2.remove_liquidity(&contract_addr, &lp_shares, &min_amount_a, &min_amount_b);

        assert!(received_a >= min_amount_a, "slippage: token_a below minimum");
        assert!(received_b >= min_amount_b, "slippage: token_b below minimum");

        // 3. Compute optimal tick range
        let range = Self::preview_range(env.clone(), received_a, received_b);
        let liquidity = range.estimated_liquidity.max(1);

        // 4. Mint V3 position
        let v3 = ClPoolClient::new(&env, &v3_pool_addr);
        let mint_result =
            v3.mint_position(&contract_addr, &range.lower_tick, &range.upper_tick, &liquidity);

        // 5. Return dust to LP
        let dust_a = received_a.saturating_sub(mint_result.amount_a).max(0);
        let dust_b = received_b.saturating_sub(mint_result.amount_b).max(0);

        if dust_a > 0 {
            TokenClient::new(&env, &token_a_addr).transfer(&contract_addr, &lp, &dust_a);
        }
        if dust_b > 0 {
            TokenClient::new(&env, &token_b_addr).transfer(&contract_addr, &lp, &dust_b);
        }

        env.events().publish(
            (Symbol::new(&env, "migrate"), lp),
            (lp_shares, mint_result.position_id, mint_result.amount_a, mint_result.amount_b, dust_a, dust_b),
        );

        MigrateResult {
            v3_position_id: mint_result.position_id,
            amount_a_deposited: mint_result.amount_a,
            amount_b_deposited: mint_result.amount_b,
            dust_a,
            dust_b,
            lower_tick: range.lower_tick,
            upper_tick: range.upper_tick,
        }
    }

    /// Compute the tick range that `migrate` would use for the given amounts.
    pub fn preview_range(env: Env, amount_a: i128, amount_b: i128) -> RangeResult {
        let v3_pool_addr: Address = env.storage().instance().get(&DataKey::V3Pool).unwrap();
        let v3 = ClPoolClient::new(&env, &v3_pool_addr);
        let state = v3.get_pool_state();

        let current_tick = state.current_tick;

        let ratio_skew = if amount_a > 0 && amount_b > 0 {
            ((amount_a.max(amount_b) * 10) / amount_a.min(amount_b).max(1)).min(50) as i32
        } else {
            10_i32
        };

        let half_width = 500_i32 + ratio_skew * 50;
        let lower_tick = (current_tick - half_width).max(-887_200);
        let upper_tick = (current_tick + half_width).min(887_200);

        let estimated_liquidity = Self::amounts_to_liquidity(
            amount_a, amount_b, lower_tick, upper_tick, state.sqrt_price_x96,
        );

        RangeResult { lower_tick, upper_tick, estimated_liquidity }
    }

    fn amounts_to_liquidity(
        amount_a: i128,
        amount_b: i128,
        lower_tick: i32,
        upper_tick: i32,
        sqrt_price: i128,
    ) -> i128 {
        let sqrt_lower = tick_to_sqrt_price_x96(lower_tick);
        let sqrt_upper = tick_to_sqrt_price_x96(upper_tick);

        if sqrt_price <= sqrt_lower {
            amount_a
                .checked_mul(sqrt_lower)
                .unwrap_or(i128::MAX)
                .checked_div((sqrt_upper - sqrt_lower).max(1))
                .unwrap_or(1)
                .max(1)
        } else if sqrt_price >= sqrt_upper {
            amount_b
                .checked_mul(Q96)
                .unwrap_or(i128::MAX)
                .checked_div((sqrt_upper - sqrt_lower).max(1))
                .unwrap_or(1)
                .max(1)
        } else {
            let liq_a = if sqrt_upper > sqrt_price {
                amount_a
                    .checked_mul(sqrt_price)
                    .unwrap_or(i128::MAX)
                    .checked_div((sqrt_upper - sqrt_price).max(1))
                    .unwrap_or(1)
            } else {
                1
            };
            let liq_b = if sqrt_price > sqrt_lower {
                amount_b
                    .checked_mul(Q96)
                    .unwrap_or(i128::MAX)
                    .checked_div((sqrt_price - sqrt_lower).max(1))
                    .unwrap_or(1)
            } else {
                1
            };
            liq_a.min(liq_b).max(1)
        }
    }
}
