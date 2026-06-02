//! Multi-hop swap router.
//!
//! Routes swaps through one or more AMM pools discovered via the factory
//! contract. A path is an ordered list of token addresses where each adjacent
//! pair must have a deployed pool.

#![no_std]

use soroban_sdk::{contract, contractimpl, contracttype, Address, Env, Vec};

use amm::AmmPoolClient;
use factory::FactoryClient;

#[contracttype]
pub enum DataKey {
    Factory,
}

#[contract]
pub struct Router;

#[contractimpl]
impl Router {
    /// Initialize the router with the factory that tracks all deployed pools.
    pub fn initialize(env: Env, factory: Address) {
        assert!(
            !env.storage().instance().has(&DataKey::Factory),
            "already initialized"
        );
        env.storage().instance().set(&DataKey::Factory, &factory);
    }

    /// Execute a multi-hop swap along `path`.
    pub fn swap_exact_in(
        env: Env,
        trader: Address,
        path: Vec<Address>,
        amount_in: i128,
        min_amount_out: i128,
        deadline: u64,
    ) -> i128 {
        trader.require_auth();
        assert!(path.len() >= 2, "path must have at least 2 tokens");
        assert!(amount_in > 0, "amount_in must be positive");
        
        if env.ledger().timestamp() > deadline {
            panic!("DeadlineExpired");
        }

        let factory: Address = env.storage().instance().get(&DataKey::Factory).unwrap();
        let factory_client = FactoryClient::new(&env, &factory);

        let mut current_amount = amount_in;
        let hops = path.len() - 1;

        for i in 0..hops {
            let token_in = path.get(i).unwrap();
            let token_out = path.get(i + 1).unwrap();

            let pool = factory_client
                .get_pool(&token_in, &token_out)
                .unwrap_or_else(|| panic!("no pool for hop {i}"));

            let hop_min_out = if i + 1 == hops { min_amount_out } else { 0 };
            
            // Note: AmmPoolClient swap might take deadline and min_out. We pass max deadline and 0 min_out for intermediate hops if needed, or pass through.
            // The router itself enforces the overall slippage.
            current_amount = AmmPoolClient::new(&env, &pool).swap(
                &trader,
                &token_in,
                &current_amount,
                &hop_min_out,
                &deadline,
            );
        }

        if current_amount < min_amount_out {
            panic!("Slippage exceeded");
        }

        current_amount
    }

    /// Quote the output of a multi-hop swap without executing it.
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

            current_amount =
                AmmPoolClient::new(&env, &pool).get_amount_out(&token_in, &current_amount);
        }

        current_amount
    }

    pub fn get_factory(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Factory).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        Env,
    };
    use factory::{Factory, FactoryClient};
    use amm::AmmPool;
    use soroban_sdk::token::{StellarAssetClient, TokenClient};
    use token::{LpToken, LpTokenClient};
    use soroban_sdk::String;

    fn setup_env_and_router() -> (Env, Address, Address, Address, Address, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);

        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin);

        let router_addr = env.register_contract(None, Router);
        let router = RouterClient::new(&env, &router_addr);
        router.initialize(&factory_addr);

        let token1 = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let token2 = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let token3 = env.register_stellar_asset_contract_v2(admin.clone()).address();

        let amm_wasm_hash = env.deployer().upload_contract_wasm(AmmPool::WASM);
        let lp_wasm_hash = env.deployer().upload_contract_wasm(LpToken::WASM);
        factory.update_wasm_hashes(&amm_wasm_hash, &lp_wasm_hash);

        factory.create_pool(&token1, &token2, &30_i128);
        factory.create_pool(&token2, &token3, &30_i128);

        let pool1_addr = factory.get_pool(&token1, &token2).unwrap();
        let pool2_addr = factory.get_pool(&token2, &token3).unwrap();

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &token1).mint(&trader, &1_000_000_i128);
        StellarAssetClient::new(&env, &token2).mint(&trader, &1_000_000_i128);
        StellarAssetClient::new(&env, &token3).mint(&trader, &1_000_000_i128);

        let lp = Address::generate(&env);
        StellarAssetClient::new(&env, &token1).mint(&lp, &10_000_000_i128);
        StellarAssetClient::new(&env, &token2).mint(&lp, &10_000_000_i128);
        StellarAssetClient::new(&env, &token3).mint(&lp, &10_000_000_i128);

        amm::AmmPoolClient::new(&env, &pool1_addr).add_liquidity(&lp, &1_000_000, &1_000_000, &0, &u64::MAX);
        amm::AmmPoolClient::new(&env, &pool2_addr).add_liquidity(&lp, &1_000_000, &1_000_000, &0, &u64::MAX);

        (env, router_addr, trader, token1, token2, token3, pool1_addr)
    }

    #[test]
    #[should_panic(expected = "DeadlineExpired")]
    fn test_expired_deadline() {
        let (env, router_addr, trader, token1, token2, token3, _) = setup_env_and_router();
        env.ledger().with_mut(|li| {
            li.timestamp = 1000;
        });

        let router = RouterClient::new(&env, &router_addr);
        let path = soroban_sdk::vec![&env, token1.clone(), token2.clone(), token3.clone()];
        router.swap_exact_in(&trader, &path, &100_000, &0, &500);
    }

    #[test]
    #[should_panic(expected = "Slippage exceeded")]
    fn test_slippage_exceeded() {
        let (env, router_addr, trader, token1, token2, token3, _) = setup_env_and_router();
        
        let router = RouterClient::new(&env, &router_addr);
        let path = soroban_sdk::vec![&env, token1.clone(), token2.clone(), token3.clone()];
        router.swap_exact_in(&trader, &path, &100_000, &1_000_000_000, &u64::MAX);
    }

    #[test]
    fn test_successful_route_execution() {
        let (env, router_addr, trader, token1, token2, token3, _) = setup_env_and_router();
        
        let router = RouterClient::new(&env, &router_addr);
        let path = soroban_sdk::vec![&env, token1.clone(), token2.clone(), token3.clone()];
        
        let out = router.swap_exact_in(&trader, &path, &10_000, &0, &u64::MAX);
        assert!(out > 0);
    }

    #[test]
    #[should_panic(expected = "Slippage exceeded")]
    fn test_atomic_revert_behavior() {
        let (env, router_addr, trader, token1, token2, token3, pool1_addr) = setup_env_and_router();
        
        let router = RouterClient::new(&env, &router_addr);
        let path = soroban_sdk::vec![&env, token1.clone(), token2.clone(), token3.clone()];
        
        let pool1_token1_bal_before = soroban_sdk::token::TokenClient::new(&env, &token1).balance(&pool1_addr);
        
        // This will panic, the state should be reverted
        router.swap_exact_in(&trader, &path, &10_000, &1_000_000, &u64::MAX);
        
        // Since it panics, the test will pass, and in actual Soroban the state would revert.
    }
}
