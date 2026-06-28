//! Integration tests for the V2-to-V3 migration contract.
//!
//! Covers all scenarios from issue #364:
//!   1. Happy path — full migration returns expected V3 position
//!   2. Slippage exceeded — revert when computed amounts fall below minimums
//!   3. Zero shares — migration rejects 0 LP share input
//!   4. Invalid range — preview_range always returns lower_tick < upper_tick
//!   5. Dust is returned to the LP when range asymmetry leaves leftover tokens
//!   6. Unauthorized pool — wrong token pair causes contract to panic
//!   7. preview_range returns the same range that migrate would use

use soroban_sdk::{
    testutils::Address as _,
    token::{StellarAssetClient, TokenClient},
    Address, Env, String,
};
use amm::{AmmPool, AmmPoolClient};
use concentrated_liquidity::{ClPool, ClPoolClient, MIN_SQRT_PRICE_X96, MAX_SQRT_PRICE_X96};
use token::{LpToken, LpTokenClient};
use v2_to_v3_migration::{V2ToV3Migration, V2ToV3MigrationClient};

// ── Fixture ───────────────────────────────────────────────────────────────────

struct Fixture<'a> {
    env: Env,
    lp: Address,
    token_a: TokenClient<'a>,
    token_b: TokenClient<'a>,
    v2_lp: LpTokenClient<'a>,
    v2: AmmPoolClient<'a>,
    v3: ClPoolClient<'a>,
    migration: V2ToV3MigrationClient<'a>,
}

fn create_sac<'a>(
    env: &'a Env,
    admin: &Address,
) -> (TokenClient<'a>, StellarAssetClient<'a>) {
    let c = env.register_stellar_asset_contract_v2(admin.clone());
    (TokenClient::new(env, &c.address()), StellarAssetClient::new(env, &c.address()))
}

impl<'a> Fixture<'a> {
    fn setup(env: &'a Env) -> Self {
        env.mock_all_auths();

        let admin = Address::generate(env);
        let lp = Address::generate(env);

        let (ta, ta_sac) = create_sac(env, &admin);
        let (tb, tb_sac) = create_sac(env, &admin);

        // V2 pool
        let v2_addr = env.register_contract(None, AmmPool);
        let v2_lp_addr = env.register_contract(None, LpToken);
        LpTokenClient::new(env, &v2_lp_addr).initialize(
            &v2_addr,
            &String::from_str(env, "V2 LP"),
            &String::from_str(env, "V2LP"),
            &7u32,
        );
        let v2 = AmmPoolClient::new(env, &v2_addr);
        v2.initialize(&ta.address, &tb.address, &v2_lp_addr, &30_i128);

        ta_sac.mint(&admin, &10_000_000_i128);
        tb_sac.mint(&admin, &10_000_000_i128);
        v2.add_liquidity(&admin, &5_000_000_i128, &5_000_000_i128, &0_i128);

        ta_sac.mint(&lp, &2_000_000_i128);
        tb_sac.mint(&lp, &2_000_000_i128);
        v2.add_liquidity(&lp, &1_000_000_i128, &1_000_000_i128, &0_i128);

        // V3 pool
        let v3_addr = env.register_contract(None, ClPool);
        let v3_lp_addr = env.register_contract(None, LpToken);
        LpTokenClient::new(env, &v3_lp_addr).initialize(
            &v3_addr,
            &String::from_str(env, "V3 LP"),
            &String::from_str(env, "V3LP"),
            &7u32,
        );
        let v3 = ClPoolClient::new(env, &v3_addr);
        let mid = MIN_SQRT_PRICE_X96 + (MAX_SQRT_PRICE_X96 - MIN_SQRT_PRICE_X96) / 2;
        v3.initialize(&ta.address, &tb.address, &v3_lp_addr, &mid, &30_i128);

        // Migration contract
        let migration_addr = env.register_contract(None, V2ToV3Migration);
        let migration = V2ToV3MigrationClient::new(env, &migration_addr);
        migration.initialize(
            &admin, &v2_addr, &v3_addr, &v2_lp_addr,
            &ta.address, &tb.address, &0_i128,
        );

        Fixture {
            env: env.clone(),
            lp,
            token_a: ta,
            token_b: tb,
            v2_lp: LpTokenClient::new(env, &v2_lp_addr),
            v2,
            v3,
            migration,
        }
    }
}

// ── Test 1: Happy path ────────────────────────────────────────────────────────

#[test]
fn test_happy_path_migrate_returns_v3_position() {
    let env = Env::default();
    let f = Fixture::setup(&env);

    let lp_shares = f.v2_lp.balance(&f.lp);
    assert!(lp_shares > 0);

    let result = f.migration.migrate(&f.lp, &lp_shares, &0_i128, &0_i128);

    assert!(result.v3_position_id >= 0);
    assert!(
        result.amount_a_deposited > 0 || result.amount_b_deposited > 0,
        "at least one token must be deposited into V3"
    );
    assert_eq!(f.v2_lp.balance(&f.lp), 0, "V2 LP shares should be burned");

    let pos = f.v3.get_position(&result.v3_position_id);
    assert!(pos.liquidity > 0);
    assert!(result.lower_tick < result.upper_tick);
}

// ── Test 2: Slippage exceeded ─────────────────────────────────────────────────

#[test]
#[should_panic(expected = "slippage")]
fn test_slippage_exceeded_reverts() {
    let env = Env::default();
    let f = Fixture::setup(&env);

    let lp_shares = f.v2_lp.balance(&f.lp);
    f.migration.migrate(&f.lp, &lp_shares, &i128::MAX, &0_i128);
}

// ── Test 3: Zero shares ───────────────────────────────────────────────────────

#[test]
#[should_panic(expected = "lp_shares must be positive")]
fn test_zero_shares_reverts() {
    let env = Env::default();
    let f = Fixture::setup(&env);

    f.migration.migrate(&f.lp, &0_i128, &0_i128, &0_i128);
}

// ── Test 4: preview_range always returns valid range ─────────────────────────

#[test]
fn test_preview_range_always_returns_valid_range() {
    let env = Env::default();
    let f = Fixture::setup(&env);

    let cases: &[(i128, i128)] = &[
        (500_000, 500_000),
        (1_000_000, 1),
        (1, 1_000_000),
        (100, 100_000),
    ];

    for &(a, b) in cases {
        let range = f.migration.preview_range(&a, &b);
        assert!(
            range.lower_tick < range.upper_tick,
            "invalid range for ({}, {}): lower={} upper={}",
            a, b, range.lower_tick, range.upper_tick
        );
        assert!(range.lower_tick >= -887_200 && range.upper_tick <= 887_200);
    }
}

// ── Test 5: Dust returned ─────────────────────────────────────────────────────

#[test]
fn test_dust_returned_to_lp() {
    let env = Env::default();
    let f = Fixture::setup(&env);

    let lp_shares = f.v2_lp.balance(&f.lp);
    let before_a = f.token_a.balance(&f.lp);
    let before_b = f.token_b.balance(&f.lp);

    let result = f.migration.migrate(&f.lp, &lp_shares, &0_i128, &0_i128);

    let returned_a = f.token_a.balance(&f.lp) - before_a;
    let returned_b = f.token_b.balance(&f.lp) - before_b;

    assert_eq!(result.dust_a, returned_a, "dust_a must match actual token_a returned");
    assert_eq!(result.dust_b, returned_b, "dust_b must match actual token_b returned");
    assert!(result.dust_a >= 0);
    assert!(result.dust_b >= 0);
}

// ── Test 6: Unauthorized pool pair reverts ────────────────────────────────────

#[test]
#[should_panic]
fn test_unauthorized_pool_pair_reverts() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let lp = Address::generate(&env);

    let (ta, ta_sac) = create_sac(&env, &admin);
    let (tb, tb_sac) = create_sac(&env, &admin);

    let v2_addr = env.register_contract(None, AmmPool);
    let v2_lp_addr = env.register_contract(None, LpToken);
    LpTokenClient::new(&env, &v2_lp_addr).initialize(
        &v2_addr,
        &String::from_str(&env, "V2 LP"),
        &String::from_str(&env, "V2LP"),
        &7u32,
    );
    let v2 = AmmPoolClient::new(&env, &v2_addr);
    v2.initialize(&ta.address, &tb.address, &v2_lp_addr, &30_i128);
    ta_sac.mint(&admin, &2_000_000_i128);
    tb_sac.mint(&admin, &2_000_000_i128);
    v2.add_liquidity(&admin, &1_000_000_i128, &1_000_000_i128, &0_i128);
    ta_sac.mint(&lp, &1_000_000_i128);
    tb_sac.mint(&lp, &1_000_000_i128);
    v2.add_liquidity(&lp, &500_000_i128, &500_000_i128, &0_i128);

    // Deploy a V3 pool with a different token pair (token_a + token_c)
    let (tc, tc_sac) = create_sac(&env, &admin);
    tc_sac.mint(&admin, &1_000_000_i128);

    let v3_bad_addr = env.register_contract(None, ClPool);
    let v3_bad_lp_addr = env.register_contract(None, LpToken);
    LpTokenClient::new(&env, &v3_bad_lp_addr).initialize(
        &v3_bad_addr,
        &String::from_str(&env, "BAD LP"),
        &String::from_str(&env, "BADLP"),
        &7u32,
    );
    let mid = MIN_SQRT_PRICE_X96 + (MAX_SQRT_PRICE_X96 - MIN_SQRT_PRICE_X96) / 2;
    ClPoolClient::new(&env, &v3_bad_addr).initialize(
        &ta.address, &tc.address, &v3_bad_lp_addr, &mid, &30_i128,
    );

    let migration_addr = env.register_contract(None, V2ToV3Migration);
    let migration = V2ToV3MigrationClient::new(&env, &migration_addr);
    migration.initialize(
        &admin, &v2_addr, &v3_bad_addr, &v2_lp_addr,
        &ta.address, &tb.address, &0_i128,
    );

    let shares = LpTokenClient::new(&env, &v2_lp_addr).balance(&lp);
    migration.migrate(&lp, &shares, &0_i128, &0_i128);
}

// ── Test 7: preview_range matches migrate ─────────────────────────────────────

#[test]
fn test_preview_range_matches_migrate() {
    let env = Env::default();
    let f = Fixture::setup(&env);

    let lp_shares = f.v2_lp.balance(&f.lp);
    let v2_info = f.v2.get_info();
    let expected_a = lp_shares * v2_info.reserve_a / v2_info.total_shares;
    let expected_b = lp_shares * v2_info.reserve_b / v2_info.total_shares;

    let preview = f.migration.preview_range(&expected_a, &expected_b);
    let result = f.migration.migrate(&f.lp, &lp_shares, &0_i128, &0_i128);

    assert_eq!(result.lower_tick, preview.lower_tick, "lower_tick must match preview_range");
    assert_eq!(result.upper_tick, preview.upper_tick, "upper_tick must match preview_range");
}
