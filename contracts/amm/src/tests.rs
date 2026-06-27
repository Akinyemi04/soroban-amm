use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{StellarAssetClient, TokenClient as StellarTokenClient},
    Env,
};
use token::LpToken;

// ── Flash-loan test helper ────────────────────────────────────────────────────

#[contracttype]
enum ReceiverDataKey {
    Amm,
    TokenA,
    TokenB,
    ShouldRepay,
}

#[contract]
pub(crate) struct MockFlashLoanReceiver;

#[contractimpl]
impl MockFlashLoanReceiver {
    pub fn initialize(
        env: Env,
        amm: Address,
        token_a: Address,
        token_b: Address,
        should_repay: bool,
    ) {
        env.storage().instance().set(&ReceiverDataKey::Amm, &amm);
        env.storage()
            .instance()
            .set(&ReceiverDataKey::TokenA, &token_a);
        env.storage()
            .instance()
            .set(&ReceiverDataKey::TokenB, &token_b);
        env.storage()
            .instance()
            .set(&ReceiverDataKey::ShouldRepay, &should_repay);
    }

    pub fn on_flash_loan(
        env: Env,
        amount_a: i128,
        amount_b: i128,
        fee_a: i128,
        fee_b: i128,
        _data: Bytes,
    ) -> bool {
        let should_repay = env
            .storage()
            .instance()
            .get(&ReceiverDataKey::ShouldRepay)
            .unwrap_or(false);
        if should_repay {
            let amm: Address = env.storage().instance().get(&ReceiverDataKey::Amm).unwrap();
            let token_a: Address = env
                .storage()
                .instance()
                .get(&ReceiverDataKey::TokenA)
                .unwrap();
            let token_b: Address = env
                .storage()
                .instance()
                .get(&ReceiverDataKey::TokenB)
                .unwrap();
            if amount_a > 0 || fee_a > 0 {
                SepTokenClient::new(&env, &token_a).transfer(
                    &env.current_contract_address(),
                    &amm,
                    &(amount_a + fee_a),
                );
            }
            if amount_b > 0 || fee_b > 0 {
                SepTokenClient::new(&env, &token_b).transfer(
                    &env.current_contract_address(),
                    &amm,
                    &(amount_b + fee_b),
                );
            }
        }
        true
    }
}

// ── Test helpers ──────────────────────────────────────────────────────────────

/// Register a Stellar Asset Contract and return (TokenClient, StellarAssetClient).
pub(crate) fn create_sac<'a>(
    env: &'a Env,
    admin: &Address,
) -> (StellarTokenClient<'a>, StellarAssetClient<'a>) {
    let contract = env.register_stellar_asset_contract_v2(admin.clone());
    (
        StellarTokenClient::new(env, &contract.address()),
        StellarAssetClient::new(env, &contract.address()),
    )
}

pub(crate) struct TestSetup {
    pub(crate) env: Env,
    pub(crate) amm_addr: Address,
    pub(crate) lp_addr: Address,
    pub(crate) ta_addr: Address,
    pub(crate) tb_addr: Address,
    #[allow(dead_code)]
    pub(crate) admin: Address,
}

/// Minimal setup: env + uninitialized AMM + LP token. Tokens are created by
/// individual tests so each test can control the pool ratio independently.
pub(crate) fn setup() -> (Env, Address, Address, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(12345);
    let admin = Address::generate(&env);
    let amm_addr = env.register_contract(None, AmmPool);
    let lp_addr = env.register_contract(None, LpToken);

    token::LpTokenClient::new(&env, &lp_addr).initialize(
        &amm_addr,
        &soroban_sdk::String::from_str(&env, "AMM LP Token"),
        &soroban_sdk::String::from_str(&env, "ALP"),
        &7u32,
    );
    (env, admin.clone(), amm_addr, lp_addr, admin)
}

pub(crate) fn setup_pool(fee_bps: i128) -> TestSetup {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(12345);
    let admin = Address::generate(&env);
    let amm_addr = env.register_contract(None, AmmPool);
    let lp_addr = env.register_contract(None, LpToken);

    token::LpTokenClient::new(&env, &lp_addr).initialize(
        &amm_addr,
        &soroban_sdk::String::from_str(&env, "AMM LP Token"),
        &soroban_sdk::String::from_str(&env, "ALP"),
        &7u32,
    );

    let (ta, ta_sac) = create_sac(&env, &admin);
    let (tb, tb_sac) = create_sac(&env, &admin);

    AmmPoolClient::new(&env, &amm_addr).initialize(
        &admin,
        &ta.address,
        &tb.address,
        &lp_addr,
        &fee_bps,
        &admin,
        &0_i128,
    );

    let ta_addr = ta.address.clone();
    let tb_addr = tb.address.clone();
    drop((ta, ta_sac, tb, tb_sac));

    TestSetup {
        env,
        amm_addr,
        lp_addr,
        ta_addr,
        tb_addr,
        admin,
    }
}

// ── Call builders ─────────────────────────────────────────────────────────────
//
// Each builder captures the required arguments and applies sensible defaults
// for optional parameters (min_shares=0, min_out=0, deadline=u64::MAX).
// When the AMM interface grows a new optional parameter, only the builder
// default needs updating — existing test call sites remain untouched.

pub(crate) struct AddLiquidity<'a> {
    amm: &'a AmmPoolClient<'a>,
    provider: &'a Address,
    amount_a: i128,
    amount_b: i128,
    min_shares: i128,
    deadline: u64,
}

impl<'a> AddLiquidity<'a> {
    pub(crate) fn new(
        amm: &'a AmmPoolClient<'a>,
        provider: &'a Address,
        amount_a: i128,
        amount_b: i128,
    ) -> Self {
        Self {
            amm,
            provider,
            amount_a,
            amount_b,
            min_shares: 0,
            deadline: u64::MAX,
        }
    }

    pub(crate) fn min_shares(mut self, v: i128) -> Self {
        self.min_shares = v;
        self
    }

    pub(crate) fn execute(self) -> i128 {
        self.amm.add_liquidity(
            self.provider,
            &self.amount_a,
            &self.amount_b,
            &self.min_shares,
            &self.deadline,
        )
    }

    pub(crate) fn try_execute(
        self,
    ) -> Result<Result<i128, soroban_sdk::Error>, Result<AmmError, soroban_sdk::InvokeError>> {
        self.amm.try_add_liquidity(
            self.provider,
            &self.amount_a,
            &self.amount_b,
            &self.min_shares,
            &self.deadline,
        )
    }
}

pub(crate) struct Swap<'a> {
    amm: &'a AmmPoolClient<'a>,
    trader: &'a Address,
    token_in: &'a Address,
    amount_in: i128,
    min_out: i128,
    deadline: u64,
}

impl<'a> Swap<'a> {
    pub(crate) fn new(
        amm: &'a AmmPoolClient<'a>,
        trader: &'a Address,
        token_in: &'a Address,
        amount_in: i128,
    ) -> Self {
        Self {
            amm,
            trader,
            token_in,
            amount_in,
            min_out: 0,
            deadline: u64::MAX,
        }
    }

    pub(crate) fn min_out(mut self, v: i128) -> Self {
        self.min_out = v;
        self
    }

    pub(crate) fn execute(self) -> i128 {
        self.amm.swap(
            self.trader,
            self.token_in,
            &self.amount_in,
            &self.min_out,
            &self.deadline,
        )
    }

    pub(crate) fn try_execute(
        self,
    ) -> Result<Result<i128, soroban_sdk::Error>, Result<AmmError, soroban_sdk::InvokeError>> {
        self.amm.try_swap(
            self.trader,
            self.token_in,
            &self.amount_in,
            &self.min_out,
            &self.deadline,
        )
    }
}

pub(crate) struct SwapExactOut<'a> {
    amm: &'a AmmPoolClient<'a>,
    trader: &'a Address,
    token_out: &'a Address,
    amount_out: i128,
    max_in: i128,
    deadline: u64,
}

impl<'a> SwapExactOut<'a> {
    pub(crate) fn new(
        amm: &'a AmmPoolClient<'a>,
        trader: &'a Address,
        token_out: &'a Address,
        amount_out: i128,
        max_in: i128,
    ) -> Self {
        Self {
            amm,
            trader,
            token_out,
            amount_out,
            max_in,
            deadline: u64::MAX,
        }
    }

    pub(crate) fn execute(self) -> i128 {
        self.amm.swap_exact_out(
            self.trader,
            self.token_out,
            &self.amount_out,
            &self.max_in,
            &self.deadline,
        )
    }

    pub(crate) fn try_execute(
        self,
    ) -> Result<Result<i128, soroban_sdk::Error>, Result<AmmError, soroban_sdk::InvokeError>> {
        self.amm.try_swap_exact_out(
            self.trader,
            self.token_out,
            &self.amount_out,
            &self.max_in,
            &self.deadline,
        )
    }
}

pub(crate) struct RemoveLiquidity<'a> {
    amm: &'a AmmPoolClient<'a>,
    provider: &'a Address,
    shares: i128,
    min_a: i128,
    min_b: i128,
    deadline: u64,
}

impl<'a> RemoveLiquidity<'a> {
    pub(crate) fn new(
        amm: &'a AmmPoolClient<'a>,
        provider: &'a Address,
        shares: i128,
    ) -> Self {
        Self {
            amm,
            provider,
            shares,
            min_a: 0,
            min_b: 0,
            deadline: u64::MAX,
        }
    }

    pub(crate) fn min_a(mut self, v: i128) -> Self {
        self.min_a = v;
        self
    }

    pub(crate) fn execute(self) -> (i128, i128) {
        self.amm.remove_liquidity(
            self.provider,
            &self.shares,
            &self.min_a,
            &self.min_b,
            &self.deadline,
        )
    }

    pub(crate) fn try_execute(
        self,
    ) -> Result<Result<(i128, i128), soroban_sdk::Error>, Result<AmmError, soroban_sdk::InvokeError>>
    {
        self.amm.try_remove_liquidity(
            self.provider,
            &self.shares,
            &self.min_a,
            &self.min_b,
            &self.deadline,
        )
    }
}

// ── Initialization ────────────────────────────────────────────────────────────

// Issue #86: initialize() must persist admin, fee_recipient, and protocol_fee_bps.
#[test]
fn test_initialize_stores_admin() {
    let (env, admin, amm_addr, lp_addr, _) = setup();
    let (ta, _) = create_sac(&env, &admin);
    let (tb, _) = create_sac(&env, &admin);
    let fee_recipient = Address::generate(&env);
    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(
        &admin,
        &ta.address,
        &tb.address,
        &lp_addr,
        &30_i128,
        &fee_recipient,
        &5_i128,
    );
    let (stored_recipient, stored_bps) = amm.get_protocol_fee();
    assert_eq!(stored_recipient, Some(fee_recipient));
    assert_eq!(stored_bps, 5);
}

#[test]
fn test_set_protocol_fee_works_after_initialize() {
    let (env, admin, amm_addr, lp_addr, _) = setup();
    let (ta, _) = create_sac(&env, &admin);
    let (tb, _) = create_sac(&env, &admin);
    let fee_recipient = Address::generate(&env);
    let new_recipient = Address::generate(&env);
    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(
        &admin,
        &ta.address,
        &tb.address,
        &lp_addr,
        &30_i128,
        &fee_recipient,
        &0_i128,
    );
    amm.set_protocol_fee(&admin, &new_recipient, &10_i128);
    let (stored_recipient, stored_bps) = amm.get_protocol_fee();
    assert_eq!(stored_recipient, Some(new_recipient));
    assert_eq!(stored_bps, 10);
}

#[test]
fn test_add_and_swap() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &2_000_000_i128);

    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 2_000_000).execute();
    assert!(shares > 0);

    let info = amm.get_info();
    assert_eq!(info.reserve_a, 1_000_000);
    assert_eq!(info.reserve_b, 2_000_000);
    assert_eq!(info.flash_loan_fee_bps, 30);

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &100_000_i128);
    let out = Swap::new(&amm, &trader, &ts.ta_addr, 100_000).execute();
    assert!(out > 0);
    assert!(out < 200_000);
}

#[test]
fn test_price_ratio() {
    let (env, admin, amm_addr, lp_addr, _) = setup();

    let (ta_client, ta_sac) = create_sac(&env, &admin);
    let (tb_client, tb_sac) = create_sac(&env, &admin);

    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(
        &admin,
        &ta_client.address,
        &tb_client.address,
        &lp_addr,
        &30_i128,
        &admin,
        &0_i128,
    );

    let provider = Address::generate(&env);
    ta_sac.mint(&provider, &2_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);

    AddLiquidity::new(&amm, &provider, 2_000_000, 1_000_000).execute();

    // reserve_a = 2_000_000, reserve_b = 1_000_000
    // price_a = 1_000_000 * 1_000_000 / 2_000_000 = 500_000
    // price_b = 2_000_000 * 1_000_000 / 1_000_000 = 2_000_000
    let (price_a, price_b) = amm.price_ratio();
    assert_eq!(price_a, 500_000);
    assert_eq!(price_b, 2_000_000);
}

#[test]
fn test_price_ratio_errors_on_empty_pool() {
    let (env, admin, amm_addr, lp_addr, _) = setup();

    let (ta_client, _) = create_sac(&env, &admin);
    let (tb_client, _) = create_sac(&env, &admin);

    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(
        &admin,
        &ta_client.address,
        &tb_client.address,
        &lp_addr,
        &30_i128,
        &admin,
        &0_i128,
    );

    // No liquidity added — reserves are zero, should return typed error
    let result = amm.try_price_ratio();
    assert_eq!(result, Err(Ok(AmmError::EmptyPool)));
}

#[test]
fn test_remove_liquidity() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);

    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();
    let (out_a, out_b) = RemoveLiquidity::new(&amm, &provider, shares).execute();
    assert!(out_a > 0 && out_b > 0);
    // 1_000 shares are permanently locked on first deposit (MINIMUM_LIQUIDITY).
    assert_eq!(amm.get_info().total_shares, 1_000);
}

#[test]
fn test_initialize_twice_panics() {
    let ts = setup_pool(30);
    let amm = AmmPoolClient::new(&ts.env, &ts.amm_addr);
    let result = amm.try_initialize(
        &ts.admin,
        &ts.ta_addr,
        &ts.tb_addr,
        &ts.lp_addr,
        &30_i128,
        &ts.admin,
        &0_i128,
    );
    assert!(result.is_err());
}

#[test]
fn test_invalid_fee_panics() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let amm_addr = env.register_contract(None, AmmPool);
    let lp_addr = env.register_contract(None, LpToken);
    token::LpTokenClient::new(&env, &lp_addr).initialize(
        &amm_addr,
        &soroban_sdk::String::from_str(&env, "LP"),
        &soroban_sdk::String::from_str(&env, "LP"),
        &7u32,
    );
    let (ta, _) = create_sac(&env, &admin);
    let (tb, _) = create_sac(&env, &admin);
    let result = AmmPoolClient::new(&env, &amm_addr).try_initialize(
        &admin,
        &ta.address,
        &tb.address,
        &lp_addr,
        &10_001_i128,
        &admin,
        &0_i128,
    );
    assert!(result.is_err());
}

// ── Swap ──────────────────────────────────────────────────────────────────────

#[test]
fn test_swap_b_to_a() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(env);
    tb_sac.mint(&trader, &100_000_i128);
    let out = Swap::new(&amm, &trader, &ts.tb_addr, 100_000).execute();
    assert!(out > 0 && out < 100_000);

    let info = amm.get_info();
    assert_eq!(info.reserve_b, 1_100_000);
    assert_eq!(info.reserve_a, 1_000_000 - out);
}

#[test]
fn test_swap_slippage_panics() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &100_000_i128);
    let result = Swap::new(&amm, &trader, &ts.ta_addr, 100_000)
        .min_out(200_000)
        .try_execute();
    assert!(result.is_err());
}

#[test]
fn test_fee_accrues_to_reserves() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(env);
    let amount_in = 100_000_i128;
    ta_sac.mint(&trader, &amount_in);
    let out = Swap::new(&amm, &trader, &ts.ta_addr, amount_in).execute();

    let info = amm.get_info();
    assert_eq!(info.reserve_a, 1_000_000 + amount_in);
    assert_eq!(info.reserve_b, 1_000_000 - out);
    // k must grow because fee stays in pool
    assert!(info.reserve_a * info.reserve_b > 1_000_000 * 1_000_000);
}

// ── Issue #98: swap_exact_out ─────────────────────────────────────────────────

#[test]
fn test_swap_exact_out_normal_path() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let want_out = 50_000_i128;
    let required_in = amm.get_amount_in(&ts.tb_addr, &want_out);

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &(required_in + 1_000));

    let spent = SwapExactOut::new(&amm, &trader, &ts.tb_addr, want_out, required_in + 1_000)
        .execute();

    assert_eq!(spent, required_in);
    let info = amm.get_info();
    assert_eq!(info.reserve_b, 1_000_000 - want_out);
    assert_eq!(info.reserve_a, 1_000_000 + spent);
}

#[test]
fn test_swap_exact_out_slippage_panics() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &100_000_i128);
    // max_in=1 is far too low for any real swap — must panic with slippage message
    let result = SwapExactOut::new(&amm, &trader, &ts.tb_addr, 50_000, 1).try_execute();
    assert!(result.is_err());
}

#[test]
fn test_swap_exact_out_invalid_token_panics() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let unknown = Address::generate(env);
    let trader = Address::generate(env);
    let result = SwapExactOut::new(&amm, &trader, &unknown, 1_000, i128::MAX).try_execute();
    assert!(result.is_err());
}

#[test]
fn test_swap_exact_out_paused_panics() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    amm.pause();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &100_000_i128);
    let result = SwapExactOut::new(&amm, &trader, &ts.tb_addr, 10_000, i128::MAX).try_execute();
    assert!(result.is_err());
}

#[test]
fn test_swap_exact_out_round_trip_consistent_with_get_amount_in() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &10_000_000_i128);
    tb_sac.mint(&provider, &10_000_000_i128);
    AddLiquidity::new(&amm, &provider, 10_000_000, 10_000_000).execute();

    let want_out = 500_000_i128;
    let quoted_in = amm.get_amount_in(&ts.tb_addr, &want_out);

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &quoted_in);
    let actual_in = SwapExactOut::new(&amm, &trader, &ts.tb_addr, want_out, quoted_in).execute();

    assert_eq!(actual_in, quoted_in);
}

// ── Liquidity ─────────────────────────────────────────────────────────────────

#[test]
fn test_add_liquidity_slippage_panics() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    let result = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000)
        .min_shares(i128::MAX)
        .try_execute();
    assert!(result.is_err());
}

#[test]
fn test_remove_liquidity_slippage_panics() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();
    let result = RemoveLiquidity::new(&amm, &provider, shares)
        .min_a(i128::MAX)
        .try_execute();
    assert!(result.is_err());
}

#[test]
fn test_lp_token_transfer_enables_remove() {
    // Verify fix: LP token is the single source of truth for share ownership.
    // Before fix, AMM had a stale internal Shares map that didn't update on transfers.
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let lp = token::LpTokenClient::new(env, &ts.lp_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let recipient = Address::generate(env);
    lp.transfer(&provider, &recipient, &shares);

    assert_eq!(amm.shares_of(&provider), 0);
    assert_eq!(amm.shares_of(&recipient), shares);

    let (out_a, out_b) = RemoveLiquidity::new(&amm, &recipient, shares).execute();
    assert!(out_a > 0 && out_b > 0);
    assert_eq!(amm.get_info().total_shares, 1_000);
}

#[test]
fn test_multiple_lps() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let lp1 = Address::generate(env);
    ta_sac.mint(&lp1, &1_000_000_i128);
    tb_sac.mint(&lp1, &1_000_000_i128);
    let shares1 = AddLiquidity::new(&amm, &lp1, 1_000_000, 1_000_000).execute();

    let lp2 = Address::generate(env);
    ta_sac.mint(&lp2, &500_000_i128);
    tb_sac.mint(&lp2, &500_000_i128);
    let shares2 = AddLiquidity::new(&amm, &lp2, 500_000, 500_000).execute();

    // shares1 is provider's shares (1_000 MINIMUM_LIQUIDITY locked separately),
    // so storage total = shares1 + shares2 + 1_000.
    assert_eq!(amm.get_info().total_shares, shares1 + shares2 + 1_000);

    RemoveLiquidity::new(&amm, &lp1, shares1).execute();
    RemoveLiquidity::new(&amm, &lp2, shares2).execute();
    assert_eq!(amm.get_info().total_shares, 1_000);
}

// ── Quotes ────────────────────────────────────────────────────────────────────

#[test]
fn test_get_amount_out_matches_swap() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let amount_in = 50_000_i128;
    let quoted = amm.get_amount_out(&ts.ta_addr, &amount_in);

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &amount_in);
    let actual = Swap::new(&amm, &trader, &ts.ta_addr, amount_in).execute();

    assert_eq!(quoted, actual);
}

#[test]
fn test_sequential_swaps_invariant() {
    let ts = setup_pool(30); // 0.30% fee
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    // 1. Initial liquidity
    let provider = Address::generate(env);
    let initial_amt = 1_000_000_i128;
    ta_sac.mint(&provider, &initial_amt);
    tb_sac.mint(&provider, &initial_amt);
    AddLiquidity::new(&amm, &provider, initial_amt, initial_amt).execute();

    let info = amm.get_info();
    let initial_k = info.reserve_a * info.reserve_b;
    let mut current_k = initial_k;

    // 2. Perform 10 alternating swaps
    let trader = Address::generate(env);
    let swap_amt = 10_000_i128;

    for i in 0..10 {
        if i % 2 == 0 {
            ta_sac.mint(&trader, &swap_amt);
            Swap::new(&amm, &trader, &ts.ta_addr, swap_amt).execute();
        } else {
            tb_sac.mint(&trader, &swap_amt);
            Swap::new(&amm, &trader, &ts.tb_addr, swap_amt).execute();
        }

        let new_info = amm.get_info();
        let new_k = new_info.reserve_a * new_info.reserve_b;

        assert!(
            new_k >= initial_k,
            "Invariant violated: new_k ({new_k}) < initial_k ({initial_k}) at swap {i}"
        );
        assert!(
            new_k >= current_k,
            "k decreased: new_k ({new_k}) < current_k ({current_k}) at swap {i}"
        );

        current_k = new_k;
    }

    // Final k should be strictly greater than initial k because of fees
    assert!(current_k > initial_k);
}

#[test]
fn test_get_amount_in_round_trip() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &2_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 2_000_000).execute();

    // Forward: how much B do we get for 100_000 A?
    let amount_in = 100_000_i128;
    let amount_out = amm.get_amount_out(&ts.ta_addr, &amount_in);
    assert!(amount_out > 0);

    // Reverse: how much A is needed to get exactly amount_out of B?
    let amount_in_reverse = amm.get_amount_in(&ts.tb_addr, &amount_out);

    // Due to integer rounding (+1 in get_amount_in), the reverse quote
    // should be >= the original input and at most 1 unit more.
    assert!(
        amount_in_reverse >= amount_in,
        "reverse quote should be >= original input"
    );
    assert!(
        amount_in_reverse <= amount_in + 1,
        "reverse quote should be at most 1 unit above original input"
    );
}

#[test]
fn test_remove_liquidity_emits_event() {
    use soroban_sdk::testutils::Events as _;
    use soroban_sdk::{symbol_short, vec, IntoVal};

    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);

    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();
    let (out_a, out_b) = RemoveLiquidity::new(&amm, &provider, shares).execute();

    // Find the rm_liq event among all published events
    let events = env.events().all();
    let rm_liq_event = events
        .iter()
        .find(|e| e.0 == amm.address && e.1 == vec![env, symbol_short!("rm_liq")].into_val(env))
        .expect("remove_liquidity event not found");
    let __ver_0: (u32, (Address, i128, i128, i128)) = rm_liq_event.2.into_val(env);
    assert_eq!(__ver_0.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
    let data: (Address, i128, i128, i128) = __ver_0.1;
    let expected = (provider.clone(), shares, out_a, out_b);
    assert_eq!(data, expected);
}

#[test]
fn test_swap_emits_token_out_in_event_payload() {
    use soroban_sdk::testutils::Events as _;
    use soroban_sdk::IntoVal;

    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(env);
    let amount_in = 100_000_i128;
    ta_sac.mint(&trader, &amount_in);
    let amount_out = Swap::new(&amm, &trader, &ts.ta_addr, amount_in).execute();

    let events = env.events().all();
    let swap_event = events
        .iter()
        .find(|e| {
            e.0 == amm.address
                && e.1 == (Symbol::new(env, "swap"), trader.clone()).into_val(env)
        })
        .expect("swap event not found");

    let __ver_1: (u32, (Address, i128, Address, i128)) = swap_event.2.into_val(env);
    assert_eq!(__ver_1.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
    let data: (Address, i128, Address, i128) = __ver_1.1;
    let expected = (
        ts.ta_addr.clone(),
        amount_in,
        ts.tb_addr.clone(),
        amount_out,
    );
    assert_eq!(data, expected);
}

#[test]
fn test_twap_oracle() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    // Add liquidity to set initial price (1:1)
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    // Initial state: accumulators should be 0
    let (cum_a, cum_b, last_ts) = amm.get_price_cumulative();
    assert_eq!(cum_a, 0);
    assert_eq!(cum_b, 0);
    assert!(last_ts > 0);

    // Jump 10 seconds ahead
    env.ledger().set_timestamp(last_ts + 10);

    // Swap A for B
    let trader = Address::generate(env);
    ta_sac.mint(&trader, &100_000_i128);
    Swap::new(&amm, &trader, &ts.ta_addr, 100_000).execute();

    // Accumulators should have updated: price 1_000_000 * 10 seconds = 10_000_000
    let (new_cum_a, new_cum_b, new_ts) = amm.get_price_cumulative();
    assert_eq!(new_ts, last_ts + 10);
    assert_eq!(new_cum_a, 10_000_000);
    assert_eq!(new_cum_b, 10_000_000);

    // Jump another 5 seconds
    env.ledger().set_timestamp(new_ts + 5);

    // New spot price after swap:
    // reserve_a = 1_100_000, reserve_b = 1_000_000 - out
    // Price A = (1_000_000 - out) * 1_000_000 / 1_100_000
    let info = amm.get_info();
    let expected_price_a = info.reserve_b * 1_000_000 / info.reserve_a;
    let expected_price_b = info.reserve_a * 1_000_000 / info.reserve_b;

    // Perform another swap
    tb_sac.mint(&trader, &50_000_i128);
    Swap::new(&amm, &trader, &ts.tb_addr, 50_000).execute();

    let (final_cum_a, final_cum_b, final_ts) = amm.get_price_cumulative();
    assert_eq!(final_ts, new_ts + 5);
    assert_eq!(final_cum_a, new_cum_a + expected_price_a * 5);
    assert_eq!(final_cum_b, new_cum_b + expected_price_b * 5);
}

#[test]
fn test_twal_oracle() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let (cum, last_ts) = amm.get_liquidity_cumulative();
    assert_eq!(cum, 0);
    assert!(last_ts > 0);

    env.ledger().set_timestamp(last_ts + 10);
    let trader = Address::generate(env);
    ta_sac.mint(&trader, &10_000_i128);
    Swap::new(&amm, &trader, &ts.ta_addr, 10_000).execute();

    let (new_cum, new_ts) = amm.get_liquidity_cumulative();
    assert_eq!(new_ts, last_ts + 10);
    // sqrt(1e6 * 1e6) = 1e6, * 10s = 10_000_000
    assert_eq!(new_cum, 10_000_000);
}

// ── Edge cases: zero-reserve guard ────────────────────────────────────────────

#[test]
fn test_swap_on_empty_pool_panics() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &1_000_i128);
    let result = Swap::new(&amm, &trader, &ts.ta_addr, 1_000).try_execute();
    assert!(result.is_err());
}

// ── Edge cases: fee boundary ───────────────────────────────────────────────────

#[test]
fn test_fee_bps_zero_succeeds() {
    let ts = setup_pool(0);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(env);
    let amount_in = 100_000_i128;
    ta_sac.mint(&trader, &amount_in);
    let out = Swap::new(&amm, &trader, &ts.ta_addr, amount_in).execute();
    // fee_bps=0 → no discount; pure constant-product formula
    let expected = amount_in * 1_000_000 / (1_000_000 + amount_in);
    assert_eq!(out, expected);
}

#[test]
fn test_fee_bps_max_succeeds() {
    // fee_bps=10_000 is the inclusive upper bound; pool initializes successfully.
    // With 100% fee, amount_in_with_fee = 0, so amount_out = 0.
    let ts = setup_pool(10_000);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &100_000_i128);
    let result = Swap::new(&amm, &trader, &ts.ta_addr, 100_000).try_execute();
    assert!(result.is_ok());
    assert_eq!(result.unwrap().unwrap(), 0);
}

#[test]
fn test_get_amount_in_max_fee_does_not_panic() {
    // With fee_bps=10_000 the `(10_000 - fee_bps)` divisor is zero. The quote
    // must return 0 instead of trapping on a division by zero.
    let ts = setup_pool(10_000);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let quoted_in = amm.get_amount_in(&ts.tb_addr, &1_000);
    assert_eq!(quoted_in, 0);
}

// ── Edge cases: minimum share precision ───────────────────────────────────────

#[test]
fn test_min_shares_exact_succeeds() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    // Initial deposit: sqrt(1_000_000 * 1_000_000) = 1_000_000; 1_000 locked → 999_000 to provider.
    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000)
        .min_shares(999_000)
        .execute();
    assert_eq!(shares, 999_000);
}

#[test]
fn test_min_shares_off_by_one_panics() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    // Provider receives 999_000 (1_000 locked); requesting 1_000_001 triggers the slippage guard.
    let result = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000)
        .min_shares(1_000_001)
        .try_execute();
    assert!(result.is_err());
}

// ── Issue #34: imbalanced deposit uses the minimum ratio ──────────────────────

#[test]
fn test_imbalanced_deposit_uses_minimum_ratio() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    // Seed pool: 1,000,000 A and 2,000,000 B (ratio 1:2)
    let seeder = Address::generate(env);
    ta_sac.mint(&seeder, &1_000_000_i128);
    tb_sac.mint(&seeder, &2_000_000_i128);
    AddLiquidity::new(&amm, &seeder, 1_000_000, 2_000_000).execute();

    // The contract uses storage total_shares (includes locked MINIMUM_LIQUIDITY).
    // Capture before the second deposit so the ratio reflects the seeded state.
    let storage_total = amm.get_info().total_shares;
    let shares_from_a = 500_000_i128 * storage_total / 1_000_000;
    let shares_from_b = 1_500_000_i128 * storage_total / 2_000_000;

    // Deposit 500,000 A and 1,500,000 B — B is 500,000 in excess of the 1:2 ratio
    let lp2 = Address::generate(env);
    ta_sac.mint(&lp2, &500_000_i128);
    tb_sac.mint(&lp2, &1_500_000_i128);
    let shares_minted = AddLiquidity::new(&amm, &lp2, 500_000, 1_500_000).execute();

    assert!(
        shares_from_a < shares_from_b,
        "TokenA should be the limiting ratio"
    );
    assert_eq!(
        shares_minted, shares_from_a,
        "shares minted must use the limiting (TokenA) ratio"
    );

    let info = amm.get_info();
    assert_eq!(info.reserve_a, 1_500_000);
    assert_eq!(info.reserve_b, 3_500_000);
}

// ── Issue #35: partial remove_liquidity leaves correct residual reserves ──────

#[test]
fn test_partial_remove_liquidity_leaves_correct_reserves() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    let provider_shares = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();
    // 1_000 MINIMUM_LIQUIDITY locked permanently; provider receives the rest.
    assert_eq!(provider_shares, 999_000);

    // storage total_shares = 1_000_000 (includes locked 1_000).
    let shares_to_remove = provider_shares / 4; // 249_750
    let (out_a, out_b) = RemoveLiquidity::new(&amm, &provider, shares_to_remove).execute();

    assert_eq!(out_a, 249_750);
    assert_eq!(out_b, 249_750);

    let info = amm.get_info();
    assert_eq!(info.reserve_a, 750_250);
    assert_eq!(info.reserve_b, 750_250);
    assert_eq!(info.total_shares, 750_250);
}

// ── Issue #36: swap output rate decreases as input size grows ─────────────────

#[test]
fn test_swap_output_rate_decreases_with_input_size() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let input_sizes = [1_000_i128, 10_000_i128, 100_000_i128, 500_000_i128];
    let mut prev_rate = i128::MAX;

    for &amount_in in input_sizes.iter() {
        let amount_out = amm.get_amount_out(&ts.ta_addr, &amount_in);
        // Scale by 1_000_000 to preserve precision when comparing rates
        let rate = amount_out * 1_000_000 / amount_in;
        assert!(
            rate < prev_rate,
            "effective rate {rate} at input {amount_in} should be strictly less than previous rate {prev_rate}"
        );
        prev_rate = rate;
    }
}

// ── Issue #37: overflow guard tests for near-maximum reserve values ────────────

#[test]
fn test_sqrt_handles_large_input() {
    // sqrt(10^18) = 10^9
    assert_eq!(
        AmmPool::sqrt(1_000_000_000_000_000_000_i128),
        1_000_000_000_i128
    );
    // sqrt(10^36) = 10^18; 10^36 < i128::MAX (~1.7e38)
    assert_eq!(
        AmmPool::sqrt(1_000_000_000_000_000_000_000_000_000_000_000_000_i128),
        1_000_000_000_000_000_000_i128,
    );
}

#[test]
fn test_large_reserves_add_liquidity_no_overflow() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    // 4e18 * 4e18 = 1.6e37 < i128::MAX (~1.7e38); sqrt = 4e18
    let large_amount = 4_000_000_000_000_000_000_i128;
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &large_amount);
    tb_sac.mint(&provider, &large_amount);
    let shares = AddLiquidity::new(&amm, &provider, large_amount, large_amount).execute();

    // 1_000 MINIMUM_LIQUIDITY locked on first deposit.
    assert_eq!(shares, large_amount - 1_000);
    let info = amm.get_info();
    assert_eq!(info.reserve_a, large_amount);
    assert_eq!(info.reserve_b, large_amount);
}

#[test]
fn test_large_reserves_swap_no_overflow() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let large_amount = 4_000_000_000_000_000_000_i128;
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &large_amount);
    tb_sac.mint(&provider, &large_amount);
    AddLiquidity::new(&amm, &provider, large_amount, large_amount).execute();

    // amount_in=10^9; numerator = 10^9*9970*4e18 ~ 4e31 < i128::MAX
    let trader = Address::generate(env);
    let amount_in = 1_000_000_000_i128;
    ta_sac.mint(&trader, &amount_in);
    let out = Swap::new(&amm, &trader, &ts.ta_addr, amount_in).execute();
    assert!(out > 0 && out < large_amount);
}

#[test]
fn test_large_reserves_price_ratio_no_overflow() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let large_amount = 4_000_000_000_000_000_000_i128;
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &large_amount);
    tb_sac.mint(&provider, &large_amount);
    AddLiquidity::new(&amm, &provider, large_amount, large_amount).execute();

    // price_ratio: reserve_b * 1_000_000 / reserve_a; 4e18 * 1e6 = 4e24 < i128::MAX
    let (price_a, price_b) = amm.price_ratio();
    assert_eq!(price_a, 1_000_000);
    assert_eq!(price_b, 1_000_000);
}

#[test]
fn test_large_reserves_get_amount_in_round_trip() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let large_amount = 4_000_000_000_000_000_000_i128; // 4e18
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &large_amount);
    tb_sac.mint(&provider, &large_amount);
    AddLiquidity::new(&amm, &provider, large_amount, large_amount).execute();

    // Forward: B for A
    let amount_in = 1_000_000_000_i128;
    let amount_out = amm.get_amount_out(&ts.ta_addr, &amount_in);
    assert!(amount_out > 0);

    // Reverse: A needed for B
    let amount_in_reverse = amm.get_amount_in(&ts.tb_addr, &amount_out);

    assert!(
        amount_in_reverse >= amount_in,
        "reverse quote should be >= original input"
    );
    assert!(
        amount_in_reverse <= amount_in + 1,
        "reverse quote should be at most 1 unit above original input"
    );
}

#[test]
#[should_panic]
fn test_get_amount_in_overflow() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let large_amount = 4_000_000_000_000_000_000_i128; // 4e18
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &large_amount);
    tb_sac.mint(&provider, &large_amount);
    AddLiquidity::new(&amm, &provider, large_amount, large_amount).execute();

    // 4e18 * 1e17 * 10000 = 4e39 > i128::MAX
    amm.get_amount_in(&ts.ta_addr, &100_000_000_000_000_000_i128);
}

// Issue #199: remove_liquidity_one_sided — provider receives only token_a.
#[test]
fn test_remove_liquidity_one_sided() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);
    let ta_client = StellarTokenClient::new(env, &ts.ta_addr);
    let tb_client = StellarTokenClient::new(env, &ts.tb_addr);

    // LP1 seeds the pool so LP2's internal swap has residual reserves to trade against.
    let lp1 = Address::generate(env);
    ta_sac.mint(&lp1, &2_000_000_i128);
    tb_sac.mint(&lp1, &2_000_000_i128);
    AddLiquidity::new(&amm, &lp1, 2_000_000, 2_000_000).execute();

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let ta_before = ta_client.balance(&provider);
    let tb_before = tb_client.balance(&provider);

    // Remove one-sided: provider wants only token_a.
    // min_out = 1_000_000 ensures at least the proportional withdrawal.
    let total_out = amm.remove_liquidity_one_sided(
        &provider,
        &shares,
        &ts.ta_addr,
        &1_000_000_i128,
        &u64::MAX,
    );

    let ta_after = ta_client.balance(&provider);
    let tb_after = tb_client.balance(&provider);

    // Provider received exactly total_out of token_a.
    assert_eq!(ta_after - ta_before, total_out);
    // Provider's token_b balance is unchanged — received no token_b.
    assert_eq!(tb_after, tb_before);
    // Total received is more than the proportional token_a alone because the
    // unwanted token_b was swapped internally for more token_a.
    assert!(total_out > 1_000_000);
    // LP shares are fully redeemed.
    assert_eq!(amm.shares_of(&provider), 0);
}

// Issue #199: min_out slippage guard is enforced in remove_liquidity_one_sided.
#[test]
fn test_remove_liquidity_one_sided_slippage_fails() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let lp1 = Address::generate(env);
    ta_sac.mint(&lp1, &2_000_000_i128);
    tb_sac.mint(&lp1, &2_000_000_i128);
    AddLiquidity::new(&amm, &lp1, 2_000_000, 2_000_000).execute();

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    // min_out set impossibly high — must fail.
    let result = amm.try_remove_liquidity_one_sided(
        &provider,
        &shares,
        &ts.ta_addr,
        &i128::MAX,
        &u64::MAX,
    );
    assert!(result.is_err());
}

#[test]
fn bench_swap_cost() {
    extern crate std;
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &100_000_i128);

    env.budget().reset_default();
    Swap::new(&amm, &trader, &ts.ta_addr, 100_000).execute();
    std::println!("=== SWAP BUDGET ===");
    std::println!("{}", env.budget());
}

#[test]
fn bench_add_liquidity_cost() {
    extern crate std;
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    // seed once so we hit the "else" (proportional) branch on the measured call
    let seeder = Address::generate(env);
    ta_sac.mint(&seeder, &1_000_000_i128);
    tb_sac.mint(&seeder, &1_000_000_i128);
    AddLiquidity::new(&amm, &seeder, 1_000_000, 1_000_000).execute();

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &500_000_i128);
    tb_sac.mint(&provider, &500_000_i128);

    env.budget().reset_default();
    AddLiquidity::new(&amm, &provider, 500_000, 500_000).execute();
    std::println!("=== ADD_LIQ BUDGET ===");
    std::println!("{}", env.budget());
}
