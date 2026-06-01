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
                .unwrap_or_else(|| panic!("no pool for hop {i}"));

            let hop_min_out = if i + 1 == hops { min_out } else { 0 };
            current_amount = AmmPoolClient::new(&env, &pool).swap(
                &trader,
                &token_in,
                &current_amount,
                &hop_min_out,
            );
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
