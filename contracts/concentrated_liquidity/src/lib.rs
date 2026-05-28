//! Concentrated Liquidity AMM (Uniswap v3-style tick-based ranges).
//! Standalone contract — does NOT modify the existing AMM pool.
#![no_std]
use soroban_sdk::{contract, contractimpl, contracttype, symbol_short, Address, Env, Vec};
use soroban_sdk::token::Client as TokenClient;
use soroban_sdk::{contract, contractimpl, contracttype, symbol_short, Address, Env};

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
    PositionList(Address),    // Vec<(i32, i32)> of open tick ranges per provider
    TickCumulative,           // i64 — accumulated tick * elapsed_seconds
    LastOracleTimestamp,      // u64 — last oracle update timestamp
    OraclePoint(u64),         // timestamp → i64 tick_cumulative snapshot
    SqrtPriceX96,
    Tick(i32),
    TickBitmap(i32),
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

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PoolState {
    pub sqrt_price: u128,
    pub current_tick: i32,
    pub active_liquidity: i128,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct TickInfo {
    pub liquidity_net: i128,
    pub liquidity_gross: i128,
    pub fee_growth_outside_a: i128,
    pub fee_growth_outside_b: i128,
}

#[contract]
pub struct ConcentratedLiquidity;

#[contractimpl]
impl ConcentratedLiquidity {
    /// One-time initialisation. Sets token pair, fee, and starting tick.
    pub fn initialize(
        env: Env,
        token_a: Address,
        token_b: Address,
        fee_bps: i128,
        initial_tick: i32,
    ) {
        assert!(
            !env.storage().instance().has(&DataKey::TokenA),
            "already initialized"
        );
        assert!(token_a != token_b, "tokens must differ");
        assert!((0..=10_000).contains(&fee_bps), "invalid fee_bps");
        assert!(
            (MIN_TICK..=MAX_TICK).contains(&initial_tick),
            "tick out of range"
        );
        env.storage().instance().set(&DataKey::TokenA, &token_a);
        env.storage().instance().set(&DataKey::TokenB, &token_b);
        env.storage().instance().set(&DataKey::FeeBps, &fee_bps);
        env.storage().instance().set(&DataKey::CurrentTick, &initial_tick);
        env.storage().instance().set(&DataKey::FeeGrowthGlobalA, &0_i128);
        env.storage().instance().set(&DataKey::FeeGrowthGlobalB, &0_i128);
        env.storage().instance().set(&DataKey::ActiveLiquidity, &0_i128);
        let init_ts = env.ledger().timestamp();
        env.storage().instance().set(&DataKey::TickCumulative, &0_i64);
        env.storage().instance().set(&DataKey::LastOracleTimestamp, &init_ts);
        env.storage().instance().set(&DataKey::OraclePoint(init_ts), &0_i64);
    }

    pub fn mint_position(env: Env, provider: Address, lower_tick: i32, upper_tick: i32, amount_a_desired: i128, amount_b_desired: i128, min_a: i128, min_b: i128) -> (i128, i128) {
        provider.require_auth();
        assert!(lower_tick < upper_tick, "lower_tick must be < upper_tick");
        assert!(
            lower_tick >= MIN_TICK && upper_tick <= MAX_TICK,
            "tick out of range"
        );
        assert!(amount_a_desired > 0 || amount_b_desired > 0, "zero amounts");
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
        assert!(amount_a >= min_a, "slippage: amount_a too low");
        assert!(amount_b >= min_b, "slippage: amount_b too low");
        let liquidity =
            Self::liquidity_from_amounts(current_tick, lower_tick, upper_tick, amount_a, amount_b);
        assert!(liquidity > 0, "liquidity would be zero");
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
        let mut list: Vec<(i32, i32)> = env.storage().instance().get(&list_key).unwrap_or_else(|| Vec::new(&env));
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
        env.events().publish(
            (symbol_short!("mint_pos"), provider),
            (lower_tick, upper_tick, liquidity, amount_a, amount_b),
        );
        (amount_a, amount_b)
    }

    pub fn burn_position(
        env: Env,
        provider: Address,
        lower_tick: i32,
        upper_tick: i32,
        liquidity: i128,
    ) -> (i128, i128) {
        provider.require_auth();
        assert!(liquidity > 0, "liquidity must be positive");
        let pos_key = DataKey::Position(provider.clone(), lower_tick, upper_tick);
        let mut pos: Position = env
            .storage()
            .instance()
            .get(&pos_key)
            .expect("position not found");
        assert!(pos.liquidity >= liquidity, "insufficient liquidity");
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
            Self::amounts_for_liquidity(current_tick, lower_tick, upper_tick, liquidity, liquidity);
        pos.liquidity -= liquidity;
        env.storage().instance().set(&pos_key, &pos);
        // Remove from position list when position is fully closed
        if pos.liquidity == 0 {
            let list_key = DataKey::PositionList(provider.clone());
            let list: Vec<(i32, i32)> = env.storage().instance().get(&list_key).unwrap_or_else(|| Vec::new(&env));
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
        env.events().publish(
            (symbol_short!("burn_pos"), provider),
            (lower_tick, upper_tick, liquidity, amount_a, amount_b),
        );
        (amount_a, amount_b)
    }

    pub fn collect_fees(
        env: Env,
        provider: Address,
        lower_tick: i32,
        upper_tick: i32,
    ) -> (i128, i128) {
        provider.require_auth();
        let pos_key = DataKey::Position(provider.clone(), lower_tick, upper_tick);
        let mut pos: Position = env
            .storage()
            .instance()
            .get(&pos_key)
            .expect("position not found");

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
        (total_a, total_b)
    }

    pub fn get_position(env: Env, provider: Address, lower_tick: i32, upper_tick: i32) -> Position {
        env.storage()
            .instance()
            .get(&DataKey::Position(provider, lower_tick, upper_tick))
            .expect("position not found")
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
    ) -> i128 {
        assert!(env.ledger().timestamp() <= deadline, "deadline expired");
        sender.require_auth();
        assert!(amount_in > 0, "amount_in must be positive");

        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        assert!(token_in == token_a || token_in == token_b, "invalid token_in");
        assert!(target_tick >= MIN_TICK && target_tick <= MAX_TICK, "target tick out of range");
        // Update tick accumulator before changing current tick
        let now = env.ledger().timestamp();
        let last_ts: u64 = env.storage().instance().get(&DataKey::LastOracleTimestamp).unwrap_or(now);
        let elapsed = now.saturating_sub(last_ts) as i64;
        if elapsed > 0 {
            let current_tick: i32 = env.storage().instance().get(&DataKey::CurrentTick).unwrap_or(0);
            let cum: i64 = env.storage().instance().get(&DataKey::TickCumulative).unwrap_or(0);
            let new_cum = cum + (current_tick as i64) * elapsed;
            env.storage().instance().set(&DataKey::TickCumulative, &new_cum);
            env.storage().instance().set(&DataKey::LastOracleTimestamp, &now);
            env.storage().instance().set(&DataKey::OraclePoint(now), &new_cum);
        }
        env.storage().instance().set(&DataKey::CurrentTick, &target_tick);
        TokenClient::new(&env, &token_in).transfer(&buyer, &env.current_contract_address(), &amount_in);
        let token_out = if token_in == token_a { token_b } else { token_a };
        TokenClient::new(&env, &token_out).transfer(&env.current_contract_address(), &buyer, &amount_in);
        env.events().publish((soroban_sdk::symbol_short!("swap"), buyer), (token_in, amount_in, target_tick));
        amount_in
    }

    /// Returns raw (tick_cumulative, last_timestamp) for external consumers.
    pub fn get_tick_cumulative(env: Env) -> (i64, u64) {
        let cum: i64 = env.storage().instance().get(&DataKey::TickCumulative).unwrap_or(0);
        let ts: u64 = env.storage().instance().get(&DataKey::LastOracleTimestamp).unwrap_or(0);
        (cum, ts)
    }

    /// Returns tick_cumulative at `seconds_ago` seconds in the past.
    /// Looks up the stored oracle snapshot at exactly `now - seconds_ago`.
    /// `seconds_ago == 0` returns the current cumulative value (extrapolated to now).
    pub fn observe(env: Env, seconds_ago: u64) -> i64 {
        let cum: i64 = env.storage().instance().get(&DataKey::TickCumulative).unwrap_or(0);
        let last_ts: u64 = env.storage().instance().get(&DataKey::LastOracleTimestamp).unwrap_or(0);
        let now = env.ledger().timestamp();
        let current_tick: i32 = env.storage().instance().get(&DataKey::CurrentTick).unwrap_or(0);
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
    /// Positions with zero liquidity are excluded.
    pub fn get_positions(env: Env, provider: Address) -> Vec<(i32, i32)> {
        env.storage()
            .instance()
            .get(&DataKey::PositionList(provider))
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Simulate token amounts required for a given tick range and liquidity.
    /// Pure read — does not transfer tokens or modify state.
    pub fn quote_position(env: Env, lower_tick: i32, upper_tick: i32, liquidity: i128) -> (i128, i128) {
        assert!(lower_tick < upper_tick, "lower_tick must be < upper_tick");
        assert!(lower_tick >= MIN_TICK && upper_tick <= MAX_TICK, "tick out of range");
        assert!(liquidity > 0, "liquidity must be positive");
        let current_tick: i32 = env.storage().instance().get(&DataKey::CurrentTick).unwrap_or(0);
        Self::amounts_for_liquidity(current_tick, lower_tick, upper_tick, liquidity, liquidity)
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
                    // If we hit the limit, we only swap up to the limit
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
        assert!(
            amount_out_total >= min_amount_out,
            "slippage: amount_out too low"
        );

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

        env.events().publish(
            (soroban_sdk::symbol_short!("swap"), sender),
            (
                zero_for_one,
                amount_in_actual,
                amount_out_total,
                sqrt_price_x96,
                current_tick,
            ),
        );

        amount_out_total
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

    fn amounts_for_liquidity(ct: i32, lt: i32, ut: i32, ad: i128, bd: i128) -> (i128, i128) {
        if ct < lt {
            (ad, 0)
        } else if ct >= ut {
            (0, bd)
        } else {
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
            if tick <= current_tick {
                info.fee_growth_outside_a = fg_a;
                info.fee_growth_outside_b = fg_b;
            } else {
                info.fee_growth_outside_a = 0;
                info.fee_growth_outside_b = 0;
            }
            Self::flip_tick(env, tick);
        }

        if is_upper {
            info.liquidity_net -= liquidity_delta;
        } else {
            info.liquidity_net += liquidity_delta;
        }

        if info.liquidity_gross == 0 {
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
        provider: Address,
        token_a: Address,
        token_b: Address,
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
        client.initialize(&token_a, &token_b, &fee_bps, &initial_tick);

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
            provider,
            token_a,
            token_b,
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
    #[should_panic(expected = "deadline expired")]
    fn test_deadline_expired() {
        let env = Env::default();
        let te = setup_test_env(&env, 0, 0);
        env.ledger().set_timestamp(101);
        te.client.swap(&te.provider, &true, &1000, &0, &0, &100);
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
        client.initialize(&token_a_addr, &token_b_addr, &30_i128, &0_i32);

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

        let data: (i32, i32, i128, i128, i128) = event.2.into_val(&env);
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

    fn setup(env: &Env) -> (Address, Address, ConcentratedLiquidityClient) {
        let admin = Address::generate(env);
        let token_a = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let token_b = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(env, &cl_addr);
        client.initialize(&token_a, &token_b, &30_i128, &0_i32);
        (token_a, token_b, client)
    }

    // ── Issue #183: TWAP tick accumulator ────────────────────────────────────

    #[test]
    fn tick_cumulative_advances_across_swaps() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1_000);

        let admin = Address::generate(&env);
        let token_a = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let token_b = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&token_a, &token_b, &30_i128, &10_i32);

        // Mint tokens for swapping
        StellarAssetClient::new(&env, &token_a).mint(&cl_addr, &1_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&cl_addr, &1_000_000_i128);

        let buyer = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&buyer, &10_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&buyer, &10_000_i128);

        // First swap at t=1060: tick was 10 for 60 seconds → cumulative += 10 * 60 = 600
        env.ledger().set_timestamp(1_060);
        client.swap(&buyer, &token_a, &100_i128, &20_i32);
        let (cum1, ts1) = client.get_tick_cumulative();
        assert_eq!(cum1, 600); // 10 * 60
        assert_eq!(ts1, 1_060);

        // Second swap at t=1160: tick was 20 for 100 seconds → cumulative += 20 * 100 = 2000
        env.ledger().set_timestamp(1_160);
        client.swap(&buyer, &token_b, &100_i128, &5_i32);
        let (cum2, ts2) = client.get_tick_cumulative();
        assert_eq!(cum2, 2_600); // 600 + 2000
        assert_eq!(ts2, 1_160);
    }

    #[test]
    fn observe_zero_returns_current_cumulative() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1_000);

        let admin = Address::generate(&env);
        let token_a = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let token_b = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&token_a, &token_b, &30_i128, &5_i32);

        StellarAssetClient::new(&env, &token_a).mint(&cl_addr, &1_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&cl_addr, &1_000_000_i128);
        let buyer = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&buyer, &1_000_i128);

        // At t=1100: tick was 5 for 100 seconds → expect 5*100=500 at observe(0)
        env.ledger().set_timestamp(1_100);
        client.swap(&buyer, &token_a, &100_i128, &10_i32);
        // After swap: cum=500 (from tick=5), now at tick=10
        // observe(0) should extrapolate to now: 500 + 10*(1100-1100) = 500
        let obs = client.observe(&0_u64);
        assert_eq!(obs, 500);
    }

    #[test]
    fn average_tick_from_two_observes_matches_expected_price() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1_000);

        let admin = Address::generate(&env);
        let token_a = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let token_b = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&token_a, &token_b, &30_i128, &0_i32);

        StellarAssetClient::new(&env, &token_a).mint(&cl_addr, &1_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&cl_addr, &1_000_000_i128);
        let buyer = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&buyer, &2_000_i128);

        // Swap at t=1100 → moves tick from 0 to 100; cumulative = 0*100 = 0
        env.ledger().set_timestamp(1_100);
        client.swap(&buyer, &token_a, &100_i128, &100_i32);

        // Swap at t=1200 → moves tick from 100 to 200; cumulative = 0 + 100*100 = 10_000
        env.ledger().set_timestamp(1_200);
        client.swap(&buyer, &token_a, &100_i128, &200_i32);

        // observe(200) = cum at t=1000 = 0  (before any ticks moved)
        // observe(0)   = cum at t=1200 + 200*(1200-1200) = 10_000 + 0 = 10_000
        let obs_now = client.observe(&0_u64);
        let obs_200s_ago = client.observe(&200_u64);
        let avg_tick = (obs_now - obs_200s_ago) / 200_i64;
        // avg tick over 200s: 0*100 + 100*100 = 10000 / 200 = 50
        assert_eq!(avg_tick, 50_i64);
        // price at avg tick 50 should be > 1.0
        let price = ConcentratedLiquidity::tick_to_price(50_i32);
        assert!(price > 1_000_000);
    }

    // ── Issue #184: get_positions ─────────────────────────────────────────────

    #[test]
    fn get_positions_mint_two_close_one() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let token_a = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let token_b = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&token_a, &token_b, &30_i128, &0_i32);

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &10_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&provider, &10_000_i128);

        // Mint two positions
        client.mint_position(&provider, &-100_i32, &100_i32, &5_000_i128, &5_000_i128, &0_i128, &0_i128);
        client.mint_position(&provider, &200_i32, &400_i32, &3_000_i128, &0_i128, &0_i128, &0_i128);

        let positions = client.get_positions(&provider);
        assert_eq!(positions.len(), 2);

        // Close first position
        let liq1 = client.get_position(&provider, &-100_i32, &100_i32).liquidity;
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
        let token_a = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let token_b = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        // current_tick = 0; range [100, 200] is entirely above → pure token-A position
        client.initialize(&token_a, &token_b, &30_i128, &0_i32);

        // quote_position: above-range means all in token_a → (liquidity, 0)
        let (qa, qb) = client.quote_position(&100_i32, &200_i32, &3_000_i128);
        assert_eq!(qa, 3_000_i128);
        assert_eq!(qb, 0_i128);

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &10_000_i128);

        // mint_position with the same range should consume exactly (qa, qb)
        let (ma, mb) = client.mint_position(
            &provider, &100_i32, &200_i32,
            &qa, &0_i128,
            &0_i128, &0_i128,
        );
        assert_eq!(ma, qa);
        assert_eq!(mb, qb);

        // below-range position: current_tick = 0, range [-200, -100] → pure token-B
        StellarAssetClient::new(&env, &token_b).mint(&provider, &10_000_i128);
        let (qa2, qb2) = client.quote_position(&-200_i32, &-100_i32, &2_000_i128);
        assert_eq!(qa2, 0_i128);
        assert_eq!(qb2, 2_000_i128);

        let (ma2, mb2) = client.mint_position(
            &provider, &-200_i32, &-100_i32,
            &0_i128, &qb2,
            &0_i128, &0_i128,
        );
        assert_eq!(ma2, qa2);
        assert_eq!(mb2, qb2);
    }
}
