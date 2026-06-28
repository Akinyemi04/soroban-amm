//! Concentrated-liquidity AMM (Uniswap v3-style) on Soroban.
//!
//! Prices are stored as Q64.96 fixed-point square-root values (sqrt_price_x96).
//! Ticks index discrete price levels at spacing 1 (price = 1.0001^tick).
//! Liquidity is only active between a position's [lower_tick, upper_tick).

#![no_std]

use soroban_sdk::{
    contract, contractimpl, contracttype, map, Address, Env, Map, Symbol,
};
use soroban_sdk::token::Client as TokenClient;
use token::LpTokenClient;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Minimum representable sqrt price (Q64.96); corresponds to tick ≈ -887272.
pub const MIN_SQRT_PRICE_X96: i128 = 4_295_128_739_i128;
/// Maximum sqrt price we allow given i128 headroom (tick ≈ 443636).
pub const MAX_SQRT_PRICE_X96: i128 = 1_461_446_703_485_210_i128;
pub const MIN_TICK: i32 = -887_272;
pub const MAX_TICK: i32 = 887_272;
/// Q96 scaling factor: 2^96 as i128 (fits; 2^96 ≈ 7.92e28 < 2^127).
pub const Q96: i128 = 79_228_162_514_264_337_593_543_950_336_i128;

// ── Storage keys ─────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    TokenA,
    TokenB,
    LpToken,
    SqrtPriceX96,
    CurrentTick,
    ActiveLiquidity,
    FeeGrowthGlobalA,
    FeeGrowthGlobalB,
    FeeBps,
    Positions,
    TicksMap,
    NextPosId,
}

// ── Data types ────────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub struct Position {
    pub owner: Address,
    pub lower_tick: i32,
    pub upper_tick: i32,
    pub liquidity: i128,
    pub fee_growth_inside_last_a: i128,
    pub fee_growth_inside_last_b: i128,
    pub tokens_owed_a: i128,
    pub tokens_owed_b: i128,
}

#[contracttype]
#[derive(Clone)]
pub struct TickInfo {
    pub liquidity_gross: i128,
    pub liquidity_net: i128,
    pub fee_growth_outside_a: i128,
    pub fee_growth_outside_b: i128,
    pub initialized: bool,
}

#[contracttype]
pub struct MintResult {
    pub amount_a: i128,
    pub amount_b: i128,
    pub position_id: i128,
}

#[contracttype]
pub struct BurnResult {
    pub amount_a: i128,
    pub amount_b: i128,
}

#[contracttype]
pub struct PoolState {
    pub sqrt_price_x96: i128,
    pub current_tick: i32,
    pub active_liquidity: i128,
    pub fee_bps: i128,
}

// ── Tick math helpers ─────────────────────────────────────────────────────────

/// Approximate sqrt price for a given tick using integer arithmetic.
pub fn tick_to_sqrt_price_x96(tick: i32) -> i128 {
    let base: i128 = Q96;
    if tick == 0 {
        return base;
    }
    let abs_tick = tick.unsigned_abs() as i128;
    let steps = abs_tick.min(1000);
    let mut result = base;
    for _ in 0..steps {
        if tick > 0 {
            result = result + result / 20_000;
        } else {
            result = result - result / 20_000;
        }
        if result <= 0 {
            return MIN_SQRT_PRICE_X96;
        }
    }
    result.clamp(MIN_SQRT_PRICE_X96, MAX_SQRT_PRICE_X96)
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct ClPool;

#[contractimpl]
impl ClPool {
    pub fn initialize(
        env: Env,
        token_a: Address,
        token_b: Address,
        lp_token: Address,
        initial_sqrt_price_x96: i128,
        fee_bps: i128,
    ) {
        if env.storage().instance().has(&DataKey::TokenA) {
            panic!("already initialized");
        }
        assert!(token_a != token_b, "tokens must differ");
        assert!(
            initial_sqrt_price_x96 >= MIN_SQRT_PRICE_X96
                && initial_sqrt_price_x96 <= MAX_SQRT_PRICE_X96,
            "sqrt_price out of range"
        );
        assert!(fee_bps >= 0 && fee_bps <= 10_000, "invalid fee");

        let initial_tick = Self::sqrt_price_to_tick(initial_sqrt_price_x96);

        env.storage().instance().set(&DataKey::TokenA, &token_a);
        env.storage().instance().set(&DataKey::TokenB, &token_b);
        env.storage().instance().set(&DataKey::LpToken, &lp_token);
        env.storage().instance().set(&DataKey::SqrtPriceX96, &initial_sqrt_price_x96);
        env.storage().instance().set(&DataKey::CurrentTick, &initial_tick);
        env.storage().instance().set(&DataKey::ActiveLiquidity, &0_i128);
        env.storage().instance().set(&DataKey::FeeGrowthGlobalA, &0_i128);
        env.storage().instance().set(&DataKey::FeeGrowthGlobalB, &0_i128);
        env.storage().instance().set(&DataKey::FeeBps, &fee_bps);
        env.storage().instance().set(&DataKey::NextPosId, &0_i128);

        let empty_positions: Map<i128, Position> = map![&env];
        let empty_ticks: Map<i32, TickInfo> = map![&env];
        env.storage().instance().set(&DataKey::Positions, &empty_positions);
        env.storage().instance().set(&DataKey::TicksMap, &empty_ticks);
    }

    pub fn mint_position(
        env: Env,
        owner: Address,
        lower_tick: i32,
        upper_tick: i32,
        liquidity: i128,
    ) -> MintResult {
        owner.require_auth();
        assert!(lower_tick < upper_tick, "lower_tick must be < upper_tick");
        assert!(lower_tick >= MIN_TICK, "lower_tick below minimum");
        assert!(upper_tick <= MAX_TICK, "upper_tick above maximum");
        assert!(liquidity > 0, "liquidity must be positive");

        let sqrt_price: i128 = env.storage().instance().get(&DataKey::SqrtPriceX96).unwrap();
        let current_tick: i32 = env.storage().instance().get(&DataKey::CurrentTick).unwrap();

        let sqrt_lower = tick_to_sqrt_price_x96(lower_tick);
        let sqrt_upper = tick_to_sqrt_price_x96(upper_tick);

        let (amount_a, amount_b) =
            Self::liquidity_to_amounts(liquidity, sqrt_price, sqrt_lower, sqrt_upper);

        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();

        if amount_a > 0 {
            TokenClient::new(&env, &token_a).transfer(
                &owner,
                &env.current_contract_address(),
                &amount_a,
            );
        }
        if amount_b > 0 {
            TokenClient::new(&env, &token_b).transfer(
                &owner,
                &env.current_contract_address(),
                &amount_b,
            );
        }

        Self::update_tick(&env, lower_tick, liquidity, true);
        Self::update_tick(&env, upper_tick, -liquidity, true);

        if current_tick >= lower_tick && current_tick < upper_tick {
            let active: i128 =
                env.storage().instance().get(&DataKey::ActiveLiquidity).unwrap();
            env.storage()
                .instance()
                .set(&DataKey::ActiveLiquidity, &(active + liquidity));
        }

        let fg_a: i128 = env.storage().instance().get(&DataKey::FeeGrowthGlobalA).unwrap();
        let fg_b: i128 = env.storage().instance().get(&DataKey::FeeGrowthGlobalB).unwrap();
        let (fgi_a, fgi_b) =
            Self::fee_growth_inside(&env, lower_tick, upper_tick, current_tick, fg_a, fg_b);

        let pos_id: i128 = env.storage().instance().get(&DataKey::NextPosId).unwrap();
        let position = Position {
            owner: owner.clone(),
            lower_tick,
            upper_tick,
            liquidity,
            fee_growth_inside_last_a: fgi_a,
            fee_growth_inside_last_b: fgi_b,
            tokens_owed_a: 0,
            tokens_owed_b: 0,
        };

        let mut positions: Map<i128, Position> =
            env.storage().instance().get(&DataKey::Positions).unwrap();
        positions.set(pos_id, position);
        env.storage().instance().set(&DataKey::Positions, &positions);
        env.storage().instance().set(&DataKey::NextPosId, &(pos_id + 1));

        let lp_token: Address = env.storage().instance().get(&DataKey::LpToken).unwrap();
        LpTokenClient::new(&env, &lp_token).mint(&owner, &liquidity);

        env.events().publish(
            (Symbol::new(&env, "mint_position"), owner),
            (lower_tick, upper_tick, liquidity, amount_a, amount_b),
        );

        MintResult { amount_a, amount_b, position_id: pos_id }
    }

    pub fn burn_position(env: Env, owner: Address, position_id: i128) -> BurnResult {
        owner.require_auth();

        let mut positions: Map<i128, Position> =
            env.storage().instance().get(&DataKey::Positions).unwrap();
        let mut pos = positions.get(position_id).expect("position not found");
        assert!(pos.owner == owner, "not position owner");
        assert!(pos.liquidity > 0, "position already burned");

        let sqrt_price: i128 = env.storage().instance().get(&DataKey::SqrtPriceX96).unwrap();
        let current_tick: i32 = env.storage().instance().get(&DataKey::CurrentTick).unwrap();
        let fg_a: i128 = env.storage().instance().get(&DataKey::FeeGrowthGlobalA).unwrap();
        let fg_b: i128 = env.storage().instance().get(&DataKey::FeeGrowthGlobalB).unwrap();

        let (fgi_a, fgi_b) = Self::fee_growth_inside(
            &env, pos.lower_tick, pos.upper_tick, current_tick, fg_a, fg_b,
        );

        let liquidity = pos.liquidity;
        let sqrt_lower = tick_to_sqrt_price_x96(pos.lower_tick);
        let sqrt_upper = tick_to_sqrt_price_x96(pos.upper_tick);

        let (mut amount_a, mut amount_b) =
            Self::liquidity_to_amounts(liquidity, sqrt_price, sqrt_lower, sqrt_upper);

        let uncollected_a = (fgi_a - pos.fee_growth_inside_last_a)
            .max(0)
            .checked_mul(liquidity)
            .unwrap_or(0)
            / Q96;
        let uncollected_b = (fgi_b - pos.fee_growth_inside_last_b)
            .max(0)
            .checked_mul(liquidity)
            .unwrap_or(0)
            / Q96;

        amount_a = amount_a.checked_add(pos.tokens_owed_a + uncollected_a).unwrap_or(amount_a);
        amount_b = amount_b.checked_add(pos.tokens_owed_b + uncollected_b).unwrap_or(amount_b);

        if current_tick >= pos.lower_tick && current_tick < pos.upper_tick {
            let active: i128 =
                env.storage().instance().get(&DataKey::ActiveLiquidity).unwrap();
            env.storage()
                .instance()
                .set(&DataKey::ActiveLiquidity, &(active - liquidity).max(0));
        }

        Self::update_tick(&env, pos.lower_tick, -liquidity, false);
        Self::update_tick(&env, pos.upper_tick, liquidity, false);

        pos.liquidity = 0;
        pos.tokens_owed_a = 0;
        pos.tokens_owed_b = 0;
        positions.set(position_id, pos);
        env.storage().instance().set(&DataKey::Positions, &positions);

        let lp_token: Address = env.storage().instance().get(&DataKey::LpToken).unwrap();
        LpTokenClient::new(&env, &lp_token).burn(&owner, &liquidity);

        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();

        if amount_a > 0 {
            TokenClient::new(&env, &token_a).transfer(
                &env.current_contract_address(), &owner, &amount_a,
            );
        }
        if amount_b > 0 {
            TokenClient::new(&env, &token_b).transfer(
                &env.current_contract_address(), &owner, &amount_b,
            );
        }

        env.events().publish(
            (Symbol::new(&env, "burn_position"), owner),
            (position_id, amount_a, amount_b),
        );

        BurnResult { amount_a, amount_b }
    }

    /// Swap exact `amount_in` of `token_in`.
    /// `zero_for_one`: true = swap token_a → token_b (price decreases).
    pub fn swap(
        env: Env,
        trader: Address,
        token_in: Address,
        amount_in: i128,
        zero_for_one: bool,
        min_amount_out: i128,
    ) -> i128 {
        trader.require_auth();
        assert!(amount_in > 0, "amount_in must be positive");

        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();

        if zero_for_one {
            assert!(token_in == token_a, "zero_for_one: token_in must be token_a");
        } else {
            assert!(token_in == token_b, "!zero_for_one: token_in must be token_b");
        }

        let fee_bps: i128 = env.storage().instance().get(&DataKey::FeeBps).unwrap();
        let mut sqrt_price: i128 = env.storage().instance().get(&DataKey::SqrtPriceX96).unwrap();
        let mut current_tick: i32 = env.storage().instance().get(&DataKey::CurrentTick).unwrap();
        let mut active_liquidity: i128 =
            env.storage().instance().get(&DataKey::ActiveLiquidity).unwrap();
        let mut fg_a: i128 = env.storage().instance().get(&DataKey::FeeGrowthGlobalA).unwrap();
        let mut fg_b: i128 = env.storage().instance().get(&DataKey::FeeGrowthGlobalB).unwrap();

        let fee_amount = amount_in * fee_bps / 10_000;
        let amount_in_net = amount_in - fee_amount;

        let mut remaining = amount_in_net;
        let mut total_out: i128 = 0;
        let mut crossings = 0_u32;

        while remaining > 0 && crossings < 10 {
            let next_tick = Self::next_initialized_tick(&env, current_tick, zero_for_one);
            let sqrt_next = tick_to_sqrt_price_x96(next_tick)
                .clamp(MIN_SQRT_PRICE_X96, MAX_SQRT_PRICE_X96);

            let (step_out, new_sqrt_price, consumed) = if active_liquidity > 0 {
                Self::compute_swap_step(
                    sqrt_price, sqrt_next, active_liquidity, remaining, zero_for_one,
                )
            } else {
                (0_i128, sqrt_next, remaining)
            };

            total_out += step_out;
            remaining -= consumed.min(remaining);
            sqrt_price = new_sqrt_price;

            if active_liquidity > 0 && consumed > 0 {
                let fee_per_liq = fee_amount.min(consumed) * Q96 / active_liquidity.max(1);
                if zero_for_one {
                    fg_a += fee_per_liq;
                } else {
                    fg_b += fee_per_liq;
                }
            }

            let crossed = if zero_for_one {
                sqrt_price <= sqrt_next
            } else {
                sqrt_price >= sqrt_next
            };

            if crossed && next_tick != current_tick {
                let mut ticks: Map<i32, TickInfo> =
                    env.storage().instance().get(&DataKey::TicksMap).unwrap();
                if let Some(mut tick_info) = ticks.get(next_tick) {
                    tick_info.fee_growth_outside_a = fg_a - tick_info.fee_growth_outside_a;
                    tick_info.fee_growth_outside_b = fg_b - tick_info.fee_growth_outside_b;

                    let net = tick_info.liquidity_net;
                    let delta = if zero_for_one { -net } else { net };
                    active_liquidity = (active_liquidity + delta).max(0);

                    ticks.set(next_tick, tick_info);
                    env.storage().instance().set(&DataKey::TicksMap, &ticks);
                }
                current_tick = if zero_for_one { next_tick - 1 } else { next_tick };
                crossings += 1;
            } else {
                break;
            }

            if sqrt_price <= MIN_SQRT_PRICE_X96 || sqrt_price >= MAX_SQRT_PRICE_X96 {
                break;
            }
        }

        assert!(total_out >= min_amount_out, "slippage: insufficient output");

        sqrt_price = sqrt_price.clamp(MIN_SQRT_PRICE_X96, MAX_SQRT_PRICE_X96);
        current_tick = Self::sqrt_price_to_tick(sqrt_price);

        env.storage().instance().set(&DataKey::SqrtPriceX96, &sqrt_price);
        env.storage().instance().set(&DataKey::CurrentTick, &current_tick);
        env.storage().instance().set(&DataKey::ActiveLiquidity, &active_liquidity);
        env.storage().instance().set(&DataKey::FeeGrowthGlobalA, &fg_a);
        env.storage().instance().set(&DataKey::FeeGrowthGlobalB, &fg_b);

        let token_out = if zero_for_one { token_b.clone() } else { token_a.clone() };
        TokenClient::new(&env, &token_in).transfer(
            &trader, &env.current_contract_address(), &amount_in,
        );
        if total_out > 0 {
            TokenClient::new(&env, &token_out).transfer(
                &env.current_contract_address(), &trader, &total_out,
            );
        }

        env.events().publish(
            (Symbol::new(&env, "swap"), trader),
            (token_in, amount_in, total_out, zero_for_one),
        );

        total_out
    }

    pub fn collect_fees(env: Env, owner: Address, position_id: i128) -> (i128, i128) {
        owner.require_auth();

        let current_tick: i32 = env.storage().instance().get(&DataKey::CurrentTick).unwrap();
        let fg_a: i128 = env.storage().instance().get(&DataKey::FeeGrowthGlobalA).unwrap();
        let fg_b: i128 = env.storage().instance().get(&DataKey::FeeGrowthGlobalB).unwrap();

        let mut positions: Map<i128, Position> =
            env.storage().instance().get(&DataKey::Positions).unwrap();
        let mut pos = positions.get(position_id).expect("position not found");
        assert!(pos.owner == owner, "not position owner");

        let (fgi_a, fgi_b) = Self::fee_growth_inside(
            &env, pos.lower_tick, pos.upper_tick, current_tick, fg_a, fg_b,
        );

        let owed_a = pos.tokens_owed_a
            + (fgi_a - pos.fee_growth_inside_last_a)
                .max(0)
                .checked_mul(pos.liquidity)
                .unwrap_or(0)
                / Q96;
        let owed_b = pos.tokens_owed_b
            + (fgi_b - pos.fee_growth_inside_last_b)
                .max(0)
                .checked_mul(pos.liquidity)
                .unwrap_or(0)
                / Q96;

        pos.fee_growth_inside_last_a = fgi_a;
        pos.fee_growth_inside_last_b = fgi_b;
        pos.tokens_owed_a = 0;
        pos.tokens_owed_b = 0;
        positions.set(position_id, pos);
        env.storage().instance().set(&DataKey::Positions, &positions);

        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();

        if owed_a > 0 {
            TokenClient::new(&env, &token_a).transfer(
                &env.current_contract_address(), &owner, &owed_a,
            );
        }
        if owed_b > 0 {
            TokenClient::new(&env, &token_b).transfer(
                &env.current_contract_address(), &owner, &owed_b,
            );
        }

        env.events().publish(
            (Symbol::new(&env, "collect_fees"), owner),
            (position_id, owed_a, owed_b),
        );

        (owed_a, owed_b)
    }

    pub fn get_pool_state(env: Env) -> PoolState {
        PoolState {
            sqrt_price_x96: env.storage().instance().get(&DataKey::SqrtPriceX96).unwrap(),
            current_tick: env.storage().instance().get(&DataKey::CurrentTick).unwrap(),
            active_liquidity: env.storage().instance().get(&DataKey::ActiveLiquidity).unwrap(),
            fee_bps: env.storage().instance().get(&DataKey::FeeBps).unwrap(),
        }
    }

    pub fn get_position(env: Env, position_id: i128) -> Position {
        let positions: Map<i128, Position> =
            env.storage().instance().get(&DataKey::Positions).unwrap();
        positions.get(position_id).expect("position not found")
    }

    fn sqrt_price_to_tick(sqrt_price_x96: i128) -> i32 {
        if sqrt_price_x96 <= MIN_SQRT_PRICE_X96 {
            return MIN_TICK;
        }
        if sqrt_price_x96 >= MAX_SQRT_PRICE_X96 {
            return MAX_TICK;
        }
        let ratio = (sqrt_price_x96 - Q96) * 20_000 / Q96;
        (ratio as i32).clamp(MIN_TICK, MAX_TICK)
    }

    fn liquidity_to_amounts(
        liquidity: i128,
        sqrt_price: i128,
        sqrt_lower: i128,
        sqrt_upper: i128,
    ) -> (i128, i128) {
        let (amount_a, amount_b);

        if sqrt_price <= sqrt_lower {
            amount_a = liquidity
                .checked_mul(sqrt_upper - sqrt_lower)
                .unwrap_or(i128::MAX)
                / sqrt_lower.max(1)
                / sqrt_upper.max(1)
                * Q96;
            amount_b = 0;
        } else if sqrt_price >= sqrt_upper {
            amount_a = 0;
            amount_b = liquidity
                .checked_mul(sqrt_upper - sqrt_lower)
                .unwrap_or(i128::MAX)
                / Q96;
        } else {
            amount_a = liquidity
                .checked_mul(sqrt_upper - sqrt_price)
                .unwrap_or(i128::MAX)
                / sqrt_price.max(1)
                / sqrt_upper.max(1)
                * Q96;
            amount_b = liquidity
                .checked_mul(sqrt_price - sqrt_lower)
                .unwrap_or(i128::MAX)
                / Q96;
        }

        (amount_a.max(0), amount_b.max(0))
    }

    fn compute_swap_step(
        sqrt_price_current: i128,
        sqrt_price_target: i128,
        liquidity: i128,
        amount_remaining: i128,
        zero_for_one: bool,
    ) -> (i128, i128, i128) {
        if liquidity == 0 || amount_remaining == 0 {
            return (0, sqrt_price_current, 0);
        }

        let (amount_out, new_price) = if zero_for_one {
            let price_delta = (amount_remaining * sqrt_price_current / liquidity).min(
                (sqrt_price_current - sqrt_price_target).max(0),
            );
            let new_p = (sqrt_price_current - price_delta).max(sqrt_price_target);
            let out = liquidity.checked_mul(sqrt_price_current - new_p).unwrap_or(0) / Q96;
            (out.max(0), new_p)
        } else {
            let price_delta = (amount_remaining * Q96 / liquidity).min(
                (sqrt_price_target - sqrt_price_current).max(0),
            );
            let new_p = (sqrt_price_current + price_delta).min(sqrt_price_target);
            let out = liquidity
                .checked_mul(new_p - sqrt_price_current)
                .unwrap_or(0)
                / Q96
                / Q96
                * sqrt_price_current;
            (out.max(0), new_p)
        };

        (amount_out, new_price, amount_remaining)
    }

    fn next_initialized_tick(env: &Env, current_tick: i32, zero_for_one: bool) -> i32 {
        let ticks: Map<i32, TickInfo> = env.storage().instance().get(&DataKey::TicksMap).unwrap();
        let search_range = 200_i32;

        if zero_for_one {
            let mut best = current_tick - search_range;
            for delta in 1..=search_range {
                let t = current_tick - delta;
                if let Some(info) = ticks.get(t) {
                    if info.initialized {
                        best = t;
                        break;
                    }
                }
            }
            best
        } else {
            let mut best = current_tick + search_range;
            for delta in 1..=search_range {
                let t = current_tick + delta;
                if let Some(info) = ticks.get(t) {
                    if info.initialized {
                        best = t;
                        break;
                    }
                }
            }
            best
        }
    }

    fn update_tick(env: &Env, tick: i32, liquidity_delta: i128, is_lower: bool) {
        let mut ticks: Map<i32, TickInfo> =
            env.storage().instance().get(&DataKey::TicksMap).unwrap();

        let mut info = ticks.get(tick).unwrap_or(TickInfo {
            liquidity_gross: 0,
            liquidity_net: 0,
            fee_growth_outside_a: 0,
            fee_growth_outside_b: 0,
            initialized: false,
        });

        let abs_delta = liquidity_delta.unsigned_abs() as i128;
        if liquidity_delta > 0 {
            info.liquidity_gross += abs_delta;
        } else {
            info.liquidity_gross = (info.liquidity_gross - abs_delta).max(0);
        }

        if is_lower {
            info.liquidity_net += liquidity_delta;
        } else {
            info.liquidity_net -= liquidity_delta;
        }

        info.initialized = info.liquidity_gross > 0;
        ticks.set(tick, info);
        env.storage().instance().set(&DataKey::TicksMap, &ticks);
    }

    fn fee_growth_inside(
        env: &Env,
        lower_tick: i32,
        upper_tick: i32,
        current_tick: i32,
        fg_a: i128,
        fg_b: i128,
    ) -> (i128, i128) {
        let ticks: Map<i32, TickInfo> = env.storage().instance().get(&DataKey::TicksMap).unwrap();

        let lower_info = ticks.get(lower_tick).unwrap_or(TickInfo {
            liquidity_gross: 0,
            liquidity_net: 0,
            fee_growth_outside_a: 0,
            fee_growth_outside_b: 0,
            initialized: false,
        });
        let upper_info = ticks.get(upper_tick).unwrap_or(TickInfo {
            liquidity_gross: 0,
            liquidity_net: 0,
            fee_growth_outside_a: 0,
            fee_growth_outside_b: 0,
            initialized: false,
        });

        let (fg_below_a, fg_below_b) = if current_tick >= lower_tick {
            (lower_info.fee_growth_outside_a, lower_info.fee_growth_outside_b)
        } else {
            (fg_a - lower_info.fee_growth_outside_a, fg_b - lower_info.fee_growth_outside_b)
        };

        let (fg_above_a, fg_above_b) = if current_tick < upper_tick {
            (upper_info.fee_growth_outside_a, upper_info.fee_growth_outside_b)
        } else {
            (fg_a - upper_info.fee_growth_outside_a, fg_b - upper_info.fee_growth_outside_b)
        };

        (
            (fg_a - fg_below_a - fg_above_a).max(0),
            (fg_b - fg_below_b - fg_above_b).max(0),
        )
    }
}
