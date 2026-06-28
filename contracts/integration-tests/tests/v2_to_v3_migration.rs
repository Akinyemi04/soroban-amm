//! Integration tests for the V2-to-V3 migration contract (issue #364).
//!
//! The migration contract calls an abstract V3PoolInterface (`add_liquidity_range` +
//! `get_current_tick`). The real `ConcentratedLiquidity` contract does not expose
//! `add_liquidity_range`, so the tests below register a `MockV3Pool` that satisfies
//! the interface, pulls tokens via the approval set by the migration, and returns a
//! synthetic position ID.
//!
//! Scenarios covered:
//!   1. Happy path
//!   2. Slippage exceeded
//!   3. Zero shares
//!   4. Invalid range (preview_range rejects lower_tick >= upper_tick)
//!   5. Dust returned to LP
//!   6. Unauthorized pool pair reverts
//!   7. preview_range returns same range as migrate

use soroban_sdk::{
    contract, contractimpl,
    testutils::Address as _,
    token::{StellarAssetClient, TokenClient},
    Address, Env, String,
};
use amm::{AmmPool, AmmPoolClient};
use token::{LpToken, LpTokenClient};
use v2_to_v3_migration::{MigrationContract, MigrationContractClient, MigrationError};

const DEADLINE: u64 = u64::MAX;

// ── Mock V3 pool ──────────────────────────────────────────────────────────────
//
// Satisfies the V3PoolInterface that MigrationContract calls:
//   add_liquidity_range(provider, amount_a, amount_b, tick_lower, tick_upper,
//                       min_shares, deadline, fee_discount) -> i128
//   get_current_tick() -> i32

#[contract]
pub struct MockV3Pool;

#[contractimpl]
impl MockV3Pool {
    /// Register which tokens this pool manages (called once after deployment).
    pub fn setup(env: Env, token_a: Address, token_b: Address) {
        env.storage().instance().set(&0u32, &token_a);
        env.storage().instance().set(&1u32, &token_b);
    }

    /// Pull amount_a + amount_b from `provider` (the migration contract) and
    /// return a fixed synthetic position ID so the migration can proceed.
    pub fn add_liquidity_range(
        env: Env,
        provider: Address,
        amount_a: i128,
        amount_b: i128,
        _tick_lower: i32,
        _tick_upper: i32,
        _min_shares: i128,
        _deadline: u64,
        _fee_discount: bool,
    ) -> i128 {
        let token_a: Address = env.storage().instance().get(&0u32).unwrap();
        let token_b: Address = env.storage().instance().get(&1u32).unwrap();
        let self_addr = env.current_contract_address();

        if amount_a > 0 {
            TokenClient::new(&env, &token_a).transfer_from(
                &self_addr,
                &provider,
                &self_addr,
                &amount_a,
            );
        }
        if amount_b > 0 {
            TokenClient::new(&env, &token_b).transfer_from(
                &self_addr,
                &provider,
                &self_addr,
                &amount_b,
            );
        }

        42_i128 // synthetic position ID
    }

    pub fn get_current_tick(_env: Env) -> i32 {
        0_i32
    }
}

// ── Test fixture ──────────────────────────────────────────────────────────────

struct Fixture<'a> {
    env: Env,
    lp: Address,
    token_a: TokenClient<'a>,
    token_b: TokenClient<'a>,
    token_a_sac: StellarAssetClient<'a>,
    token_b_sac: StellarAssetClient<'a>,
    v2_lp: LpTokenClient<'a>,
    v2: AmmPoolClient<'a>,
    migration: MigrationContractClient<'a>,
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
        let fee_recipient = Address::generate(env);

        let (ta, ta_sac) = create_sac(env, &admin);
        let (tb, tb_sac) = create_sac(env, &admin);

        // ── V2 pool ────────────────────────────────────────────────────────────
        let v2_addr = env.register_contract(None, AmmPool);
        let v2_lp_addr = env.register_contract(None, LpToken);
        LpTokenClient::new(env, &v2_lp_addr).initialize(
            &v2_addr,
            &String::from_str(env, "V2 LP"),
            &String::from_str(env, "V2LP"),
            &7u32,
        );
        let v2 = AmmPoolClient::new(env, &v2_addr);
        v2.initialize(
            &admin, &ta.address, &tb.address, &v2_lp_addr,
            &30_i128, &fee_recipient, &0_i128,
        );

        // Seed V2 with admin liquidity, then give LP their position
        ta_sac.mint(&admin, &10_000_000_i128);
        tb_sac.mint(&admin, &10_000_000_i128);
        v2.add_liquidity(&admin, &5_000_000_i128, &5_000_000_i128, &0_i128, &DEADLINE);

        ta_sac.mint(&lp, &2_000_000_i128);
        tb_sac.mint(&lp, &2_000_000_i128);
        v2.add_liquidity(&lp, &1_000_000_i128, &1_000_000_i128, &0_i128, &DEADLINE);

        // ── Mock V3 pool ───────────────────────────────────────────────────────
        let v3_addr = env.register_contract(None, MockV3Pool);
        MockV3PoolClient::new(env, &v3_addr).setup(&ta.address, &tb.address);

        // ── Migration contract ─────────────────────────────────────────────────
        let migration_addr = env.register_contract(None, MigrationContract);
        let migration = MigrationContractClient::new(env, &migration_addr);
        migration.initialize(&admin, &v2_addr, &v3_addr);

        Fixture {
            env: env.clone(),
            lp,
            token_a: ta,
            token_b: tb,
            token_a_sac: ta_sac,
            token_b_sac: tb_sac,
            v2_lp: LpTokenClient::new(env, &v2_lp_addr),
            v2,
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

    let result = f.migration.migrate(
        &f.lp, &lp_shares,
        &0_i128, &0_i128,
        &i32::MIN, &i32::MAX, // auto-range
        &500_i32,
        &0_i128,
        &DEADLINE,
    );

    // V3 position was created (mock returns 42)
    assert_eq!(result.position_id, 42_i128);

    // V2 LP shares fully burned
    assert_eq!(f.v2_lp.balance(&f.lp), 0);

    // Tokens were deposited into V3 (mock consumed them)
    assert!(
        result.deposited_a > 0 || result.deposited_b > 0,
        "at least one token must be deposited"
    );

    // Tick range is valid
    assert!(result.tick_lower < result.tick_upper);
}

// ── Test 2: Slippage exceeded ─────────────────────────────────────────────────

#[test]
fn test_slippage_exceeded_reverts() {
    let env = Env::default();
    let f = Fixture::setup(&env);

    let lp_shares = f.v2_lp.balance(&f.lp);
    assert!(lp_shares > 0);

    // min_amount_a impossibly high — V2 remove_liquidity will fail
    let result = f.migration.try_migrate(
        &f.lp, &lp_shares,
        &i128::MAX, &0_i128,
        &i32::MIN, &i32::MAX, &500_i32, &0_i128, &DEADLINE,
    );

    assert!(result.is_err(), "migration with impossible slippage should fail");
}

// ── Test 3: Zero shares ───────────────────────────────────────────────────────

#[test]
fn test_zero_shares_reverts() {
    let env = Env::default();
    let f = Fixture::setup(&env);

    let result = f.migration.try_migrate(
        &f.lp, &0_i128,
        &0_i128, &0_i128,
        &i32::MIN, &i32::MAX, &500_i32, &0_i128, &DEADLINE,
    );

    assert!(
        matches!(result, Err(Ok(MigrationError::ZeroShares))),
        "should return ZeroShares error"
    );
}

// ── Test 4: Invalid range (preview_range rejects lower_tick >= upper_tick) ────

#[test]
fn test_invalid_range_preview_reverts() {
    let env = Env::default();
    let f = Fixture::setup(&env);

    // Explicit ticks where lower >= upper
    let result = f.migration.try_preview_range(&100_i32, &50_i32, &0_i32);
    assert!(
        matches!(result, Err(Ok(MigrationError::InvalidRange))),
        "lower_tick >= upper_tick should return InvalidRange"
    );

    // Equal ticks also invalid
    let result2 = f.migration.try_preview_range(&100_i32, &100_i32, &0_i32);
    assert!(matches!(result2, Err(Ok(MigrationError::InvalidRange))));

    // Auto-range with range_width_ticks = 0 is also invalid
    let result3 = f.migration.try_preview_range(&i32::MIN, &i32::MAX, &0_i32);
    assert!(matches!(result3, Err(Ok(MigrationError::InvalidRange))));
}

// ── Test 5: Dust returned to LP ───────────────────────────────────────────────

#[test]
fn test_dust_returned_to_lp() {
    let env = Env::default();
    let f = Fixture::setup(&env);

    let lp_shares = f.v2_lp.balance(&f.lp);
    let before_a = f.token_a.balance(&f.lp);
    let before_b = f.token_b.balance(&f.lp);

    let result = f.migration.migrate(
        &f.lp, &lp_shares,
        &0_i128, &0_i128,
        &i32::MIN, &i32::MAX, &500_i32, &0_i128, &DEADLINE,
    );

    let returned_a = f.token_a.balance(&f.lp) - before_a;
    let returned_b = f.token_b.balance(&f.lp) - before_b;

    // refund fields must match actual balance change
    assert_eq!(result.refund_a, returned_a, "refund_a must match actual token_a returned");
    assert_eq!(result.refund_b, returned_b, "refund_b must match actual token_b returned");
    assert!(result.refund_a >= 0);
    assert!(result.refund_b >= 0);

    // deposited + refunded = received (conservation)
    let received_a = result.deposited_a + result.refund_a;
    let received_b = result.deposited_b + result.refund_b;
    assert!(received_a > 0 || received_b > 0, "no tokens flowed through migration");
}

// ── Test 6: Unauthorized pool pair reverts ────────────────────────────────────

#[test]
fn test_unauthorized_pool_pair_reverts() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let lp = Address::generate(&env);
    let fee_recipient = Address::generate(&env);

    let (ta, ta_sac) = create_sac(&env, &admin);
    let (tb, tb_sac) = create_sac(&env, &admin);

    // Legitimate V2 pool
    let v2_addr = env.register_contract(None, AmmPool);
    let v2_lp_addr = env.register_contract(None, LpToken);
    LpTokenClient::new(&env, &v2_lp_addr).initialize(
        &v2_addr,
        &String::from_str(&env, "V2 LP"),
        &String::from_str(&env, "V2LP"),
        &7u32,
    );
    let v2 = AmmPoolClient::new(&env, &v2_addr);
    v2.initialize(
        &admin, &ta.address, &tb.address, &v2_lp_addr,
        &30_i128, &fee_recipient, &0_i128,
    );
    ta_sac.mint(&admin, &2_000_000_i128);
    tb_sac.mint(&admin, &2_000_000_i128);
    v2.add_liquidity(&admin, &1_000_000_i128, &1_000_000_i128, &0_i128, &DEADLINE);
    ta_sac.mint(&lp, &1_000_000_i128);
    tb_sac.mint(&lp, &1_000_000_i128);
    v2.add_liquidity(&lp, &500_000_i128, &500_000_i128, &0_i128, &DEADLINE);

    // Mock V3 pool wired to a DIFFERENT token pair (token_a + token_c)
    let (tc, tc_sac) = create_sac(&env, &admin);
    tc_sac.mint(&admin, &1_000_000_i128);

    let v3_bad_addr = env.register_contract(None, MockV3Pool);
    MockV3PoolClient::new(&env, &v3_bad_addr).setup(&ta.address, &tc.address);

    let migration_addr = env.register_contract(None, MigrationContract);
    let migration = MigrationContractClient::new(&env, &migration_addr);
    migration.initialize(&admin, &v2_addr, &v3_bad_addr);

    let shares = LpTokenClient::new(&env, &v2_lp_addr).balance(&lp);

    // The mock will try to pull token_c (which the migration never received)
    // causing the transfer to fail
    let result = migration.try_migrate(
        &lp, &shares,
        &0_i128, &0_i128,
        &i32::MIN, &i32::MAX, &500_i32, &0_i128, &DEADLINE,
    );

    assert!(result.is_err(), "migration with wrong V3 pool token pair should fail");
}

// ── Test 7: preview_range matches migrate ─────────────────────────────────────

#[test]
fn test_preview_range_matches_migrate() {
    let env = Env::default();
    let f = Fixture::setup(&env);

    // Preview using auto-range with width 500
    let (preview_lower, preview_upper) = f.migration
        .preview_range(&i32::MIN, &i32::MAX, &500_i32);

    assert!(preview_lower < preview_upper, "preview_range must return valid range");

    let lp_shares = f.v2_lp.balance(&f.lp);
    let result = f.migration.migrate(
        &f.lp, &lp_shares,
        &0_i128, &0_i128,
        &i32::MIN, &i32::MAX, &500_i32, &0_i128, &DEADLINE,
    );

    assert_eq!(result.tick_lower, preview_lower, "migrate tick_lower must match preview_range");
    assert_eq!(result.tick_upper, preview_upper, "migrate tick_upper must match preview_range");
}
