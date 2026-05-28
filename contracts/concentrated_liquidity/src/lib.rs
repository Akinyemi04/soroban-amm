//! Minimal concentrated-liquidity (V3-style) scaffold on Soroban.
//!
//! This crate is a scaffold: it tracks the global price/fee state that any
//! V3-style pool exposes (`sqrt_price_x96`, `tick`, and the per-token global
//! fee accumulators) but does not yet implement tick math, position
//! management, or swap-along-ticks logic. The `swap` entrypoint is a
//! placeholder that mutates the global state so callers and tests can
//! observe slot0 transitions.

#![no_std]

use soroban_sdk::{contract, contractimpl, contracttype, Address, Env};

// ── Storage keys ─────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    Admin,
    TokenA,
    TokenB,
    FeeBps,
    SqrtPriceX96,
    Tick,
    FeeGrowthGlobalA,
    FeeGrowthGlobalB,
}

// ── Slot0 view payload ───────────────────────────────────────────────────────

#[contracttype]
pub struct Slot0 {
    pub sqrt_price_x96: i128,
    pub tick: i32,
    pub fee_growth_global_a: i128,
    pub fee_growth_global_b: i128,
}

// ── Contract ─────────────────────────────────────────────────────────────────

#[contract]
pub struct ConcentratedLiquidityPool;

#[contractimpl]
impl ConcentratedLiquidityPool {
    /// One-time pool setup. `initial_sqrt_price_x96` and `initial_tick` should
    /// agree (real implementations derive one from the other via tick math);
    /// the scaffold accepts them as-is from the caller.
    pub fn initialize(
        env: Env,
        admin: Address,
        token_a: Address,
        token_b: Address,
        fee_bps: i128,
        initial_sqrt_price_x96: i128,
        initial_tick: i32,
    ) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        assert!(token_a != token_b, "tokens must differ");
        assert!(fee_bps >= 0 && fee_bps <= 10_000, "invalid fee");
        assert!(initial_sqrt_price_x96 > 0, "sqrt_price must be positive");

        let s = env.storage().instance();
        s.set(&DataKey::Admin, &admin);
        s.set(&DataKey::TokenA, &token_a);
        s.set(&DataKey::TokenB, &token_b);
        s.set(&DataKey::FeeBps, &fee_bps);
        s.set(&DataKey::SqrtPriceX96, &initial_sqrt_price_x96);
        s.set(&DataKey::Tick, &initial_tick);
        s.set(&DataKey::FeeGrowthGlobalA, &0_i128);
        s.set(&DataKey::FeeGrowthGlobalB, &0_i128);
    }

    /// View: admin address set at initialization. Callable without auth.
    pub fn get_admin(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Admin).unwrap()
    }

    /// Placeholder swap: advances `sqrt_price_x96`, `tick`, and the input
    /// token's `fee_growth_global` so slot0 reflects the trade. No real
    /// price-impact math, no token transfers, no liquidity tracking.
    pub fn swap(env: Env, trader: Address, token_in: Address, amount_in: i128) {
        trader.require_auth();
        assert!(amount_in > 0, "amount_in must be positive");

        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let fee_bps: i128 = env.storage().instance().get(&DataKey::FeeBps).unwrap();

        let fee = amount_in * fee_bps / 10_000;
        let net = amount_in - fee;

        let mut sqrt_price: i128 = env.storage().instance().get(&DataKey::SqrtPriceX96).unwrap();
        let mut tick: i32 = env.storage().instance().get(&DataKey::Tick).unwrap();

        if token_in == token_a {
            // Selling A for B → price of B/A rises in this convention,
            // sqrt_price increases, tick moves up.
            sqrt_price = sqrt_price.checked_add(net).expect("sqrt_price overflow");
            tick = tick.checked_add(1).expect("tick overflow");
            let prev: i128 = env
                .storage()
                .instance()
                .get(&DataKey::FeeGrowthGlobalA)
                .unwrap_or(0);
            env.storage()
                .instance()
                .set(&DataKey::FeeGrowthGlobalA, &(prev + fee));
        } else if token_in == token_b {
            sqrt_price = sqrt_price.checked_sub(net).expect("sqrt_price underflow");
            assert!(sqrt_price > 0, "sqrt_price must stay positive");
            tick = tick.checked_sub(1).expect("tick underflow");
            let prev: i128 = env
                .storage()
                .instance()
                .get(&DataKey::FeeGrowthGlobalB)
                .unwrap_or(0);
            env.storage()
                .instance()
                .set(&DataKey::FeeGrowthGlobalB, &(prev + fee));
        } else {
            panic!("token_in is not part of this pool");
        }

        env.storage().instance().set(&DataKey::SqrtPriceX96, &sqrt_price);
        env.storage().instance().set(&DataKey::Tick, &tick);
    }

    /// View: current price and fee state. Callable without auth.
    pub fn get_slot0(env: Env) -> Slot0 {
        let s = env.storage().instance();
        Slot0 {
            sqrt_price_x96: s.get(&DataKey::SqrtPriceX96).unwrap(),
            tick: s.get(&DataKey::Tick).unwrap(),
            fee_growth_global_a: s.get(&DataKey::FeeGrowthGlobalA).unwrap_or(0),
            fee_growth_global_b: s.get(&DataKey::FeeGrowthGlobalB).unwrap_or(0),
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    const INITIAL_SQRT_PRICE: i128 = 79_228_162_514_264_337_593_543_950_336; // 1.0 in Q64.96
    const INITIAL_TICK: i32 = 0;
    const FEE_BPS: i128 = 30;

    fn setup() -> (Env, Address, Address, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();

        let addr = env.register_contract(None, ConcentratedLiquidityPool);
        let admin = Address::generate(&env);
        let token_a = Address::generate(&env);
        let token_b = Address::generate(&env);

        let client = ConcentratedLiquidityPoolClient::new(&env, &addr);
        client.initialize(
            &admin,
            &token_a,
            &token_b,
            &FEE_BPS,
            &INITIAL_SQRT_PRICE,
            &INITIAL_TICK,
        );

        (env, addr, admin, token_a, token_b)
    }

    #[test]
    fn get_admin_returns_initialized_admin() {
        let (env, addr, admin, _a, _b) = setup();
        let client = ConcentratedLiquidityPoolClient::new(&env, &addr);

        assert_eq!(client.get_admin(), admin);
    }

    #[test]
    fn slot0_initial_state() {
        let (env, addr, _admin, _a, _b) = setup();
        let client = ConcentratedLiquidityPoolClient::new(&env, &addr);

        let slot0 = client.get_slot0();
        assert_eq!(slot0.sqrt_price_x96, INITIAL_SQRT_PRICE);
        assert_eq!(slot0.tick, INITIAL_TICK);
        assert_eq!(slot0.fee_growth_global_a, 0);
        assert_eq!(slot0.fee_growth_global_b, 0);
    }

    #[test]
    fn slot0_after_swap_token_a_in() {
        let (env, addr, _admin, token_a, _token_b) = setup();
        let client = ConcentratedLiquidityPoolClient::new(&env, &addr);

        let trader = Address::generate(&env);
        let amount_in: i128 = 1_000_000;
        client.swap(&trader, &token_a, &amount_in);

        let expected_fee = amount_in * FEE_BPS / 10_000;
        let expected_net = amount_in - expected_fee;

        let slot0 = client.get_slot0();
        assert_eq!(slot0.sqrt_price_x96, INITIAL_SQRT_PRICE + expected_net);
        assert_eq!(slot0.tick, INITIAL_TICK + 1);
        assert_eq!(slot0.fee_growth_global_a, expected_fee);
        assert_eq!(slot0.fee_growth_global_b, 0);
    }

    #[test]
    fn slot0_after_swap_token_b_in() {
        let (env, addr, _admin, _token_a, token_b) = setup();
        let client = ConcentratedLiquidityPoolClient::new(&env, &addr);

        let trader = Address::generate(&env);
        let amount_in: i128 = 500_000;
        client.swap(&trader, &token_b, &amount_in);

        let expected_fee = amount_in * FEE_BPS / 10_000;
        let expected_net = amount_in - expected_fee;

        let slot0 = client.get_slot0();
        assert_eq!(slot0.sqrt_price_x96, INITIAL_SQRT_PRICE - expected_net);
        assert_eq!(slot0.tick, INITIAL_TICK - 1);
        assert_eq!(slot0.fee_growth_global_a, 0);
        assert_eq!(slot0.fee_growth_global_b, expected_fee);
    }
}
