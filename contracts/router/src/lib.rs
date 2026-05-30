//! Multi-hop swap router.
//!
//! Routes swaps through one or more AMM pools discovered via the factory
//! contract. A path is an ordered list of token addresses where each adjacent
//! pair must have a deployed pool.
//!
//! Flow:
//!   1. Deploy this contract.
//!   2. Call `initialize` with the factory address.
//!   3. Callers invoke `swap_exact_in` with a token path and slippage guard.
//!   4. Use `get_amount_out_path` to quote without executing.

#![no_std]

use soroban_sdk::{contract, contractclient, contractimpl, contracttype, Address, Env, Vec};

// ── External contract interfaces ─────────────────────────────────────────────

/// Minimal AMM pool interface needed by the router.
#[contractclient(name = "AmmPoolClient")]
pub trait AmmPoolInterface {
    fn swap(env: Env, trader: Address, token_in: Address, amount_in: i128, min_out: i128) -> i128;
    fn get_amount_out(env: Env, token_in: Address, amount_in: i128) -> i128;
}

/// Minimal factory interface needed by the router.
#[contractclient(name = "FactoryClient")]
pub trait FactoryInterface {
    fn get_pool(env: Env, token_a: Address, token_b: Address) -> Option<Address>;
}

// ── Storage keys ─────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    Factory,
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct Router;

#[contractimpl]
impl Router {
    /// Initialize the router with the factory that tracks all deployed pools.
    ///
    /// Must be called exactly once after deployment.
    pub fn initialize(env: Env, factory: Address) {
        assert!(
            !env.storage().instance().has(&DataKey::Factory),
            "already initialized"
        );
        env.storage().instance().set(&DataKey::Factory, &factory);
    }

    /// Execute a multi-hop swap along `path`.
    ///
    /// `path` must contain at least two token addresses. Each adjacent pair
    /// `(path[i], path[i+1])` must have a pool registered in the factory.
    /// Tokens flow from `path[0]` to `path[last]`.
    ///
    /// `min_out` is a slippage guard on the final output only; intermediate
    /// outputs are passed directly into the next hop with no guard so that
    /// the only externally observable constraint is the end-to-end amount.
    ///
    /// Requires `trader` to have authorised the call.
    ///
    /// # Returns
    /// The amount of `path[last]` received by `trader`.
    pub fn swap_exact_in(
        env: Env,
        trader: Address,
        path: Vec<Address>,
        amount_in: i128,
        min_out: i128,
    ) -> i128 {
        trader.require_auth();
        assert!(path.len() >= 2, "path must have at least 2 tokens");
        assert!(amount_in > 0, "amount_in must be positive");

        let factory: Address = env.storage().instance().get(&DataKey::Factory).unwrap();
        let factory_client = FactoryClient::new(&env, &factory);

        let mut current_amount = amount_in;
        let hops = path.len() - 1;

        for i in 0..hops {
            let token_in = path.get(i).unwrap();
            let token_out = path.get(i + 1).unwrap();

            let pool = factory_client
                .get_pool(&token_in, &token_out)
                .unwrap_or_else(|| panic!("no pool for hop {i}: {token_in:?} -> {token_out:?}"));

            let pool_client = AmmPoolClient::new(&env, &pool);

            // On the last hop apply the caller's slippage guard; intermediate
            // hops use 0 so dust rounding does not abort mid-route.
            let hop_min_out = if i + 1 == hops { min_out } else { 0 };

            current_amount =
                pool_client.swap(&trader, &token_in, &current_amount, &hop_min_out);
        }

        current_amount
    }

    /// Quote the output of a multi-hop swap without executing it.
    ///
    /// Applies each pool's current `get_amount_out` in sequence.
    /// Returns 0 if any pool in the path does not exist.
    pub fn get_amount_out_path(env: Env, path: Vec<Address>, amount_in: i128) -> i128 {
        assert!(path.len() >= 2, "path must have at least 2 tokens");
        assert!(amount_in > 0, "amount_in must be positive");

        let factory: Address = env.storage().instance().get(&DataKey::Factory).unwrap();
        let factory_client = FactoryClient::new(&env, &factory);

        let mut current_amount = amount_in;
        let hops = path.len() - 1;

        for i in 0..hops {
            let token_in = path.get(i).unwrap();
            let token_out = path.get(i + 1).unwrap();

            let pool = match factory_client.get_pool(&token_in, &token_out) {
                Some(p) => p,
                None => return 0,
            };

            current_amount = AmmPoolClient::new(&env, &pool).get_amount_out(&token_in, &current_amount);
        }

        current_amount
    }

    /// Return the factory address this router was initialized with.
    pub fn get_factory(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Factory).unwrap()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::Address as _,
        token::{StellarAssetClient, TokenClient as StellarTokenClient},
        Env,
    };

    mod amm_wasm {
        soroban_sdk::contractimport!(
            file = "../../target/wasm32-unknown-unknown/release/amm.wasm"
        );
    }

    mod token_wasm {
        soroban_sdk::contractimport!(
            file = "../../target/wasm32-unknown-unknown/release/token.wasm"
        );
    }

    mod factory_wasm {
        soroban_sdk::contractimport!(
            file = "../../target/wasm32-unknown-unknown/release/factory.wasm"
        );
    }

    fn create_sac<'a>(
        env: &'a Env,
        admin: &Address,
    ) -> (StellarTokenClient<'a>, StellarAssetClient<'a>) {
        let contract = env.register_stellar_asset_contract_v2(admin.clone());
        (
            StellarTokenClient::new(env, &contract.address()),
            StellarAssetClient::new(env, &contract.address()),
        )
    }

    /// Deploy factory + two pools (A-B and B-C) and return router client.
    struct MultiHopSetup {
        env: Env,
        router_addr: Address,
        ta: Address,
        tb: Address,
        tc: Address,
    }

    fn setup_multi_hop() -> MultiHopSetup {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1000);

        let admin = Address::generate(&env);

        let amm_hash = env.deployer().upload_contract_wasm(amm_wasm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token_wasm::WASM);

        let factory_addr = env.register_contract(None, factory_wasm::Factory);
        let factory = factory_wasm::FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let (ta_client, ta_sac) = create_sac(&env, &admin);
        let (tb_client, tb_sac) = create_sac(&env, &admin);
        let (tc_client, tc_sac) = create_sac(&env, &admin);

        let pool_ab = factory.create_pool(&ta_client.address, &tb_client.address, &30_i128);
        let pool_bc = factory.create_pool(&tb_client.address, &tc_client.address, &30_i128);

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &2_000_000_i128);
        tc_sac.mint(&provider, &1_000_000_i128);

        let ab_pool = amm_wasm::Client::new(&env, &pool_ab);
        ab_pool.add_liquidity(&provider, &1_000_000_i128, &1_000_000_i128, &0_i128);

        let bc_pool = amm_wasm::Client::new(&env, &pool_bc);
        bc_pool.add_liquidity(&provider, &1_000_000_i128, &1_000_000_i128, &0_i128);

        let router_addr = env.register_contract(None, Router);
        RouterClient::new(&env, &router_addr).initialize(&factory_addr);

        MultiHopSetup {
            env,
            router_addr,
            ta: ta_client.address,
            tb: tb_client.address,
            tc: tc_client.address,
        }
    }

    #[test]
    fn test_initialize_stores_factory() {
        let env = Env::default();
        env.mock_all_auths();
        let factory = Address::generate(&env);
        let router_addr = env.register_contract(None, Router);
        let router = RouterClient::new(&env, &router_addr);
        router.initialize(&factory);
        assert_eq!(router.get_factory(), factory);
    }

    #[test]
    fn test_initialize_twice_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let factory = Address::generate(&env);
        let router_addr = env.register_contract(None, Router);
        let router = RouterClient::new(&env, &router_addr);
        router.initialize(&factory);
        assert!(router.try_initialize(&factory).is_err());
    }

    #[test]
    fn test_direct_swap_via_router() {
        let s = setup_multi_hop();
        let env = &s.env;
        let router = RouterClient::new(env, &s.router_addr);

        let trader = Address::generate(env);
        let ta_sac = StellarAssetClient::new(env, &s.ta);
        ta_sac.mint(&trader, &100_000_i128);

        let path = soroban_sdk::vec![env, s.ta.clone(), s.tb.clone()];
        let out = router.swap_exact_in(&trader, &path, &100_000_i128, &0_i128);
        assert!(out > 0);
        assert!(out < 100_000);
    }

    #[test]
    fn test_two_hop_swap_a_to_c() {
        let s = setup_multi_hop();
        let env = &s.env;
        let router = RouterClient::new(env, &s.router_addr);

        let trader = Address::generate(env);
        let ta_sac = StellarAssetClient::new(env, &s.ta);
        ta_sac.mint(&trader, &100_000_i128);

        let path = soroban_sdk::vec![env, s.ta.clone(), s.tb.clone(), s.tc.clone()];
        let out = router.swap_exact_in(&trader, &path, &100_000_i128, &0_i128);
        assert!(out > 0);
        // Two hops with fees each: output must be less than single hop
        assert!(out < 100_000);
    }

    #[test]
    fn test_quote_matches_execution() {
        let s = setup_multi_hop();
        let env = &s.env;
        let router = RouterClient::new(env, &s.router_addr);

        let path = soroban_sdk::vec![env, s.ta.clone(), s.tb.clone(), s.tc.clone()];
        let quoted = router.get_amount_out_path(&path, &50_000_i128);

        let trader = Address::generate(env);
        StellarAssetClient::new(env, &s.ta).mint(&trader, &50_000_i128);
        let actual = router.swap_exact_in(&trader, &path, &50_000_i128, &0_i128);

        assert_eq!(quoted, actual);
    }

    #[test]
    fn test_slippage_guard_on_final_hop() {
        let s = setup_multi_hop();
        let env = &s.env;
        let router = RouterClient::new(env, &s.router_addr);

        let trader = Address::generate(env);
        StellarAssetClient::new(env, &s.ta).mint(&trader, &100_000_i128);

        let path = soroban_sdk::vec![env, s.ta.clone(), s.tb.clone()];
        // min_out set impossibly high
        let result = router.try_swap_exact_in(&trader, &path, &100_000_i128, &i128::MAX);
        assert!(result.is_err());
    }

    #[test]
    fn test_quote_returns_zero_for_missing_pool() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);

        let amm_hash = env.deployer().upload_contract_wasm(amm_wasm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token_wasm::WASM);

        let factory_addr = env.register_contract(None, factory_wasm::Factory);
        let factory = factory_wasm::FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let router_addr = env.register_contract(None, Router);
        let router = RouterClient::new(&env, &router_addr);
        router.initialize(&factory_addr);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);

        let path = soroban_sdk::vec![&env, ta, tb];
        // No pool created — should return 0
        let out = router.get_amount_out_path(&path, &1_000_i128);
        assert_eq!(out, 0);
    }

    #[test]
    fn test_two_hop_quote_less_than_single_hop() {
        let s = setup_multi_hop();
        let env = &s.env;
        let router = RouterClient::new(env, &s.router_addr);

        let amount_in = 50_000_i128;
        let single = router.get_amount_out_path(
            &soroban_sdk::vec![env, s.ta.clone(), s.tb.clone()],
            &amount_in,
        );
        let two_hop = router.get_amount_out_path(
            &soroban_sdk::vec![env, s.ta.clone(), s.tb.clone(), s.tc.clone()],
            &amount_in,
        );

        // Two hops accumulate fees, so output is lower
        assert!(two_hop < single);
    }
}
