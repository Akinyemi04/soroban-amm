//! Fuzz harness for the concentrated_liquidity contract.
//!
//! Exercises: mint_position, swap (with tick-crossing), burn_position,
//! collect_fees, and a multi-position scenario.
//!
//! Property invariants checked after every operation:
//!   1. sqrt_price_x96 ∈ [MIN_SQRT_PRICE_X96, MAX_SQRT_PRICE_X96]
//!   2. active_liquidity >= 0
//!   3. After full burn: tokens returned ≤ tokens deposited (rounds-down)
//!   4. Fee collection never panics; fees owed are non-negative

#![no_std]

extern crate alloc;

use soroban_sdk::{
    testutils::Address as _,
    token::{StellarAssetClient, TokenClient},
    Address, Env, String,
};
use concentrated_liquidity::{
    ClPool, ClPoolClient, MAX_SQRT_PRICE_X96, MIN_SQRT_PRICE_X96,
};
use token::{LpToken, LpTokenClient};

// ── Harness setup ─────────────────────────────────────────────────────────────

pub struct FuzzEnv<'a> {
    pub env: Env,
    pub pool: ClPoolClient<'a>,
    pub token_a: TokenClient<'a>,
    pub token_b: TokenClient<'a>,
    pub token_a_sac: StellarAssetClient<'a>,
    pub token_b_sac: StellarAssetClient<'a>,
    pub admin: Address,
}

impl<'a> FuzzEnv<'a> {
    pub fn setup(env: &'a Env) -> Self {
        env.mock_all_auths();

        let admin = Address::generate(env);
        let pool_addr = env.register_contract(None, ClPool);
        let lp_addr = env.register_contract(None, LpToken);

        LpTokenClient::new(env, &lp_addr).initialize(
            &pool_addr,
            &String::from_str(env, "CL LP"),
            &String::from_str(env, "CLLP"),
            &7u32,
        );

        let (ta_client, ta_sac) = create_sac(env, &admin);
        let (tb_client, tb_sac) = create_sac(env, &admin);

        ta_sac.mint(&admin, &1_000_000_000_i128);
        tb_sac.mint(&admin, &1_000_000_000_i128);

        let pool = ClPoolClient::new(env, &pool_addr);
        pool.initialize(
            &ta_client.address,
            &tb_client.address,
            &lp_addr,
            &MIN_SQRT_PRICE_X96.saturating_add((MAX_SQRT_PRICE_X96 - MIN_SQRT_PRICE_X96) / 2),
            &30_i128,
        );

        FuzzEnv {
            env: env.clone(),
            pool,
            token_a: ta_client,
            token_b: tb_client,
            token_a_sac: ta_sac,
            token_b_sac: tb_sac,
            admin,
        }
    }
}

fn create_sac<'a>(
    env: &'a Env,
    admin: &Address,
) -> (TokenClient<'a>, StellarAssetClient<'a>) {
    let contract = env.register_stellar_asset_contract_v2(admin.clone());
    (
        TokenClient::new(env, &contract.address()),
        StellarAssetClient::new(env, &contract.address()),
    )
}

// ── Property assertions ───────────────────────────────────────────────────────

fn assert_pool_invariants(fuzz: &FuzzEnv) {
    let state = fuzz.pool.get_pool_state();
    assert!(
        state.sqrt_price_x96 >= MIN_SQRT_PRICE_X96,
        "sqrt_price below minimum: {}",
        state.sqrt_price_x96
    );
    assert!(
        state.sqrt_price_x96 <= MAX_SQRT_PRICE_X96,
        "sqrt_price above maximum: {}",
        state.sqrt_price_x96
    );
    assert!(
        state.active_liquidity >= 0,
        "active_liquidity negative: {}",
        state.active_liquidity
    );
}

// ── Fuzz targets ──────────────────────────────────────────────────────────────

pub fn fuzz_mint_position(lower_tick: i32, upper_tick: i32, liquidity: i128) {
    let env = Env::default();
    let fuzz = FuzzEnv::setup(&env);

    let lower = lower_tick.clamp(-887_200, 887_199).min(upper_tick.clamp(-887_199, 887_200) - 1);
    let upper = upper_tick.clamp(lower + 1, 887_200);
    let liq = liquidity.clamp(1, 1_000_000);

    fuzz.token_a_sac.mint(&fuzz.admin, &(liq * 2));
    fuzz.token_b_sac.mint(&fuzz.admin, &(liq * 2));

    let result = fuzz.pool.mint_position(&fuzz.admin, &lower, &upper, &liq);
    assert!(result.position_id >= 0);
    assert!(result.amount_a >= 0 && result.amount_b >= 0);
    assert_pool_invariants(&fuzz);
}

pub fn fuzz_swap(zero_for_one: bool, amount_in: i128) {
    let env = Env::default();
    let fuzz = FuzzEnv::setup(&env);

    fuzz.token_a_sac.mint(&fuzz.admin, &10_000_000_i128);
    fuzz.token_b_sac.mint(&fuzz.admin, &10_000_000_i128);
    let _ = fuzz.pool.mint_position(&fuzz.admin, &-1000, &1000, &1_000_000_i128);

    let trader = Address::generate(&env);
    let swap_amount = amount_in.clamp(1, 500_000);

    if zero_for_one {
        fuzz.token_a_sac.mint(&trader, &swap_amount);
        let _ = fuzz.pool.try_swap(
            &trader, &fuzz.token_a.address, &swap_amount, &true, &0_i128,
        );
    } else {
        fuzz.token_b_sac.mint(&trader, &swap_amount);
        let _ = fuzz.pool.try_swap(
            &trader, &fuzz.token_b.address, &swap_amount, &false, &0_i128,
        );
    }

    assert_pool_invariants(&fuzz);
}

/// Seed 3 overlapping positions then sweep through all of them.
pub fn fuzz_tick_crossing(swap_amount: i128) {
    let env = Env::default();
    let fuzz = FuzzEnv::setup(&env);

    fuzz.token_a_sac.mint(&fuzz.admin, &100_000_000_i128);
    fuzz.token_b_sac.mint(&fuzz.admin, &100_000_000_i128);

    let _ = fuzz.pool.mint_position(&fuzz.admin, &-500, &500, &500_000_i128);
    let _ = fuzz.pool.mint_position(&fuzz.admin, &-200, &200, &200_000_i128);
    let _ = fuzz.pool.mint_position(&fuzz.admin, &-50, &50, &100_000_i128);

    let trader = Address::generate(&env);
    let amount = swap_amount.clamp(1, 2_000_000);
    fuzz.token_a_sac.mint(&trader, &amount);

    let _ = fuzz.pool.try_swap(&trader, &fuzz.token_a.address, &amount, &true, &0_i128);

    assert_pool_invariants(&fuzz);
}

/// Mint then immediately burn — tokens returned must not exceed tokens deposited.
pub fn fuzz_burn_after_mint(lower_tick: i32, upper_tick: i32, liquidity: i128) {
    let env = Env::default();
    let fuzz = FuzzEnv::setup(&env);

    let lower = lower_tick.clamp(-887_200, -1);
    let upper = upper_tick.clamp(1, 887_200);
    let liq = liquidity.clamp(1, 500_000);

    fuzz.token_a_sac.mint(&fuzz.admin, &(liq * 3));
    fuzz.token_b_sac.mint(&fuzz.admin, &(liq * 3));

    let before_a = fuzz.token_a.balance(&fuzz.admin);
    let before_b = fuzz.token_b.balance(&fuzz.admin);

    let mint_result = fuzz.pool.mint_position(&fuzz.admin, &lower, &upper, &liq);
    let deposited_a = mint_result.amount_a;
    let deposited_b = mint_result.amount_b;

    assert_pool_invariants(&fuzz);

    let burn_result = fuzz.pool.burn_position(&fuzz.admin, &mint_result.position_id);

    assert!(
        burn_result.amount_a <= deposited_a,
        "burn returned more token_a than deposited: {} > {}",
        burn_result.amount_a,
        deposited_a
    );
    assert!(
        burn_result.amount_b <= deposited_b,
        "burn returned more token_b than deposited: {} > {}",
        burn_result.amount_b,
        deposited_b
    );

    let net_a = before_a - fuzz.token_a.balance(&fuzz.admin);
    let net_b = before_b - fuzz.token_b.balance(&fuzz.admin);
    assert!(net_a >= 0, "admin gained token_a through mint+burn: net={}", net_a);
    assert!(net_b >= 0, "admin gained token_b through mint+burn: net={}", net_b);

    assert_pool_invariants(&fuzz);
}

/// Collect fees; second collect must return (0, 0).
pub fn fuzz_collect_fees(swap_amount: i128) {
    let env = Env::default();
    let fuzz = FuzzEnv::setup(&env);

    fuzz.token_a_sac.mint(&fuzz.admin, &100_000_000_i128);
    fuzz.token_b_sac.mint(&fuzz.admin, &100_000_000_i128);

    let mint_result = fuzz.pool.mint_position(&fuzz.admin, &-500, &500, &1_000_000_i128);
    let pos_id = mint_result.position_id;

    let trader = Address::generate(&env);
    let amount = swap_amount.clamp(1, 1_000_000);
    fuzz.token_a_sac.mint(&trader, &(amount * 5));
    fuzz.token_b_sac.mint(&trader, &(amount * 5));

    for _ in 0..5 {
        let _ = fuzz.pool.try_swap(&trader, &fuzz.token_a.address, &amount, &true, &0_i128);
        let _ = fuzz.pool.try_swap(&trader, &fuzz.token_b.address, &amount, &false, &0_i128);
    }

    let (fee_a, fee_b) = fuzz.pool.collect_fees(&fuzz.admin, &pos_id);
    assert!(fee_a >= 0, "negative fee_a: {}", fee_a);
    assert!(fee_b >= 0, "negative fee_b: {}", fee_b);

    // Idempotent: second collect must return zero
    let (fee_a2, fee_b2) = fuzz.pool.collect_fees(&fuzz.admin, &pos_id);
    assert_eq!(fee_a2, 0, "double-collect returned non-zero fee_a");
    assert_eq!(fee_b2, 0, "double-collect returned non-zero fee_b");

    assert_pool_invariants(&fuzz);
}

/// 3 overlapping positions, swap both directions, collect fees, burn all.
pub fn fuzz_multi_position_scenario(swap_amount: i128) {
    let env = Env::default();
    let fuzz = FuzzEnv::setup(&env);

    fuzz.token_a_sac.mint(&fuzz.admin, &500_000_000_i128);
    fuzz.token_b_sac.mint(&fuzz.admin, &500_000_000_i128);

    let r1 = fuzz.pool.mint_position(&fuzz.admin, &-2000, &2000, &1_000_000_i128);
    assert_pool_invariants(&fuzz);
    let r2 = fuzz.pool.mint_position(&fuzz.admin, &-500, &500, &500_000_i128);
    assert_pool_invariants(&fuzz);
    let r3 = fuzz.pool.mint_position(&fuzz.admin, &-100, &100, &200_000_i128);
    assert_pool_invariants(&fuzz);

    let trader = Address::generate(&env);
    let amount = swap_amount.clamp(100, 5_000_000);
    fuzz.token_a_sac.mint(&trader, &(amount * 3));
    fuzz.token_b_sac.mint(&trader, &(amount * 3));

    let _ = fuzz.pool.try_swap(&trader, &fuzz.token_a.address, &amount, &true, &0_i128);
    assert_pool_invariants(&fuzz);
    let _ = fuzz.pool.try_swap(&trader, &fuzz.token_b.address, &amount, &false, &0_i128);
    assert_pool_invariants(&fuzz);

    let (fa1, fb1) = fuzz.pool.collect_fees(&fuzz.admin, &r1.position_id);
    let (fa2, fb2) = fuzz.pool.collect_fees(&fuzz.admin, &r2.position_id);
    let (fa3, fb3) = fuzz.pool.collect_fees(&fuzz.admin, &r3.position_id);
    assert!(fa1 >= 0 && fb1 >= 0);
    assert!(fa2 >= 0 && fb2 >= 0);
    assert!(fa3 >= 0 && fb3 >= 0);

    let b1 = fuzz.pool.burn_position(&fuzz.admin, &r1.position_id);
    let b2 = fuzz.pool.burn_position(&fuzz.admin, &r2.position_id);
    let b3 = fuzz.pool.burn_position(&fuzz.admin, &r3.position_id);
    assert!(b1.amount_a >= 0 && b1.amount_b >= 0);
    assert!(b2.amount_a >= 0 && b2.amount_b >= 0);
    assert!(b3.amount_a >= 0 && b3.amount_b >= 0);

    assert_pool_invariants(&fuzz);
}

// ── libfuzzer entry point ─────────────────────────────────────────────────────

#[cfg(feature = "libfuzzer-sys")]
use libfuzzer_sys::fuzz_target;

#[cfg(feature = "libfuzzer-sys")]
fuzz_target!(|data: &[u8]| {
    if data.len() < 17 {
        return;
    }
    let op = data[0] % 6;
    let a = i32::from_le_bytes([data[1], data[2], data[3], data[4]]);
    let b = i32::from_le_bytes([data[5], data[6], data[7], data[8]]);
    let c = i64::from_le_bytes([
        data[9], data[10], data[11], data[12],
        data[13], data[14], data[15], data[16],
    ]) as i128;

    match op {
        0 => fuzz_mint_position(a, b, c),
        1 => fuzz_swap(a % 2 == 0, c),
        2 => fuzz_tick_crossing(c),
        3 => fuzz_burn_after_mint(a, b, c),
        4 => fuzz_collect_fees(c),
        5 => fuzz_multi_position_scenario(c),
        _ => {}
    }
});

// ── Smoke tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_mint_position() {
        fuzz_mint_position(-100, 100, 10_000);
    }

    #[test]
    fn smoke_swap_zero_for_one() {
        fuzz_swap(true, 50_000);
    }

    #[test]
    fn smoke_swap_one_for_zero() {
        fuzz_swap(false, 50_000);
    }

    #[test]
    fn smoke_tick_crossing() {
        fuzz_tick_crossing(500_000);
    }

    #[test]
    fn smoke_burn_after_mint() {
        fuzz_burn_after_mint(-200, 200, 50_000);
    }

    #[test]
    fn smoke_collect_fees() {
        fuzz_collect_fees(100_000);
    }

    #[test]
    fn smoke_multi_position_scenario() {
        fuzz_multi_position_scenario(1_000_000);
    }

    #[test]
    fn smoke_burn_roundtrip_no_net_gain() {
        fuzz_burn_after_mint(-1000, 1000, 1_000);
    }

    #[test]
    fn smoke_sqrt_price_invariant_after_large_swap() {
        fuzz_swap(true, 999_999_999);
        fuzz_swap(false, 999_999_999);
    }
}
