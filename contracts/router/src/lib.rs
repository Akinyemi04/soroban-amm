//! Multi-hop swap router.
//!
//! Resolves a two-hop path `token_in → intermediate → token_out` from the
//! factory registry and executes both swaps atomically. The trader signs the
//! top-level call; their auth tree authorizes the nested token transfers.

#![no_std]

use soroban_sdk::{contract, contractimpl, contracttype, vec, Address, Env, Symbol, Vec};

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
    pub fn initialize(env: Env, factory: Address) {
        if env.storage().instance().has(&DataKey::Factory) {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Factory, &factory);
    }

    /// Swap `amount_in` of `token_in` for at least `min_out` of `token_out`
    /// through an intermediate pool discovered via the factory.
    pub fn swap_through(
        env: Env,
        trader: Address,
        token_in: Address,
        token_out: Address,
        amount_in: i128,
        min_out: i128,
        deadline: u64,
    ) -> i128 {
        trader.require_auth();
        assert!(env.ledger().timestamp() <= deadline, "deadline expired");
        assert!(token_in != token_out, "tokens must differ");
        assert!(amount_in > 0, "amount_in must be positive");

        let (intermediate, pool_first, pool_second) =
            Self::find_two_hop_path(&env, &token_in, &token_out);

        let amount_intermediate = AmmPoolClient::new(&env, &pool_first)
            .swap(&trader, &token_in, &amount_in, &0_i128);

        let amount_out = AmmPoolClient::new(&env, &pool_second).swap(
            &trader,
            &intermediate,
            &amount_intermediate,
            &min_out,
        );

        env.events().publish(
            (Symbol::new(&env, "multi_hop_swap"), trader),
            (token_in, intermediate, token_out, amount_in, amount_out),
        );

        amount_out
    }

    /// Quote the output of a two-hop swap and return the resolved path
    /// `[token_in, intermediate, token_out]`.
    pub fn get_multi_hop_out(
        env: Env,
        token_in: Address,
        token_out: Address,
        amount_in: i128,
    ) -> (i128, Vec<Address>) {
        assert!(token_in != token_out, "tokens must differ");
        assert!(amount_in > 0, "amount_in must be positive");

        let (intermediate, pool_first, pool_second) =
            Self::find_two_hop_path(&env, &token_in, &token_out);

        let amount_intermediate =
            AmmPoolClient::new(&env, &pool_first).get_amount_out(&token_in, &amount_in);
        let amount_out = AmmPoolClient::new(&env, &pool_second)
            .get_amount_out(&intermediate, &amount_intermediate);

        let path = vec![&env, token_in, intermediate, token_out];
        (amount_out, path)
    }

    pub fn get_factory(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Factory).unwrap()
    }

    fn find_two_hop_path(
        env: &Env,
        token_in: &Address,
        token_out: &Address,
    ) -> (Address, Address, Address) {
        let factory: Address = env.storage().instance().get(&DataKey::Factory).unwrap();
        let factory_client = FactoryClient::new(env, &factory);

        let partners = factory_client.get_partners(token_in);
        for intermediate in partners.iter() {
            if &intermediate == token_out {
                continue;
            }
            let pool_first = match factory_client.get_pool(token_in, &intermediate) {
                Some(p) => p,
                None => continue,
            };
            let pool_second = match factory_client.get_pool(&intermediate, token_out) {
                Some(p) => p,
                None => continue,
            };
            return (intermediate, pool_first, pool_second);
        }
        panic!("no two-hop path found");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amm::AmmPool;
    use factory::Factory;
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        token::{StellarAssetClient, TokenClient as StellarTokenClient},
        Env, String,
    };
    use token::{LpToken, LpTokenClient};

    fn deploy_pool(env: &Env, token_a: &Address, token_b: &Address) -> Address {
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        LpTokenClient::new(env, &lp_addr).initialize(
            &amm_addr,
            &String::from_str(env, "LP"),
            &String::from_str(env, "LP"),
            &7u32,
        );
        AmmPoolClient::new(env, &amm_addr).initialize(token_a, token_b, &lp_addr, &30_i128);
        amm_addr
    }

    /// (token_a, token_b, token_c, router_address)
    fn setup_chain(env: &Env) -> (Address, Address, Address, Address) {
        let admin = Address::generate(env);

        let ta = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tb = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tc = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        let pool_ab = deploy_pool(env, &ta, &tb);
        let pool_bc = deploy_pool(env, &tb, &tc);

        let lp = Address::generate(env);
        StellarAssetClient::new(env, &ta).mint(&lp, &1_000_000_i128);
        StellarAssetClient::new(env, &tb).mint(&lp, &2_000_000_i128);
        StellarAssetClient::new(env, &tc).mint(&lp, &1_000_000_i128);
        AmmPoolClient::new(env, &pool_ab).add_liquidity(
            &lp,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
        );
        AmmPoolClient::new(env, &pool_bc).add_liquidity(
            &lp,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
        );

        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(env, &factory_addr);
        factory.initialize(&admin);
        factory.register_pool(&pool_ab, &ta, &tb);
        factory.register_pool(&pool_bc, &tb, &tc);

        let router_addr = env.register_contract(None, Router);
        RouterClient::new(env, &router_addr).initialize(&factory_addr);

        (ta, tb, tc, router_addr)
    }

    #[test]
    fn test_two_hop_swap() {
        let env = Env::default();
        env.mock_all_auths();
        let (ta, tb, tc, router_addr) = setup_chain(&env);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);

        let deadline = env.ledger().timestamp() + 100;
        let out = RouterClient::new(&env, &router_addr).swap_through(
            &trader,
            &ta,
            &tc,
            &100_000_i128,
            &0_i128,
            &deadline,
        );

        assert!(out > 0);
        assert_eq!(StellarTokenClient::new(&env, &ta).balance(&trader), 0);
        assert_eq!(StellarTokenClient::new(&env, &tb).balance(&trader), 0);
        assert_eq!(StellarTokenClient::new(&env, &tc).balance(&trader), out);
    }

    #[test]
    fn test_get_multi_hop_out_matches_swap() {
        let env = Env::default();
        env.mock_all_auths();
        let (ta, tb, tc, router_addr) = setup_chain(&env);
        let router = RouterClient::new(&env, &router_addr);

        let (quoted, path) = router.get_multi_hop_out(&ta, &tc, &100_000_i128);
        assert_eq!(path.len(), 3);
        assert_eq!(path.get(0).unwrap(), ta);
        assert_eq!(path.get(1).unwrap(), tb);
        assert_eq!(path.get(2).unwrap(), tc);
        assert!(quoted > 0);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);
        let deadline = env.ledger().timestamp() + 100;
        let actual = router.swap_through(&trader, &ta, &tc, &100_000_i128, &0_i128, &deadline);
        assert_eq!(actual, quoted);
    }

    #[test]
    #[should_panic(expected = "slippage")]
    fn test_slippage_revert() {
        let env = Env::default();
        env.mock_all_auths();
        let (ta, _tb, tc, router_addr) = setup_chain(&env);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);

        let deadline = env.ledger().timestamp() + 100;
        // Demand more than the path can possibly deliver.
        RouterClient::new(&env, &router_addr).swap_through(
            &trader,
            &ta,
            &tc,
            &100_000_i128,
            &10_000_000_i128,
            &deadline,
        );
    }

    #[test]
    #[should_panic(expected = "no two-hop path found")]
    fn test_missing_intermediate_pool() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let ta = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tb = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tc = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        // Only the A-B pool exists; nothing connects to C.
        let pool_ab = deploy_pool(&env, &ta, &tb);

        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin);
        factory.register_pool(&pool_ab, &ta, &tb);

        let router_addr = env.register_contract(None, Router);
        let router = RouterClient::new(&env, &router_addr);
        router.initialize(&factory_addr);

        let trader = Address::generate(&env);
        let deadline = env.ledger().timestamp() + 100;
        router.swap_through(&trader, &ta, &tc, &100_000_i128, &0_i128, &deadline);
    }

    #[test]
    #[should_panic(expected = "deadline expired")]
    fn test_deadline_expired() {
        let env = Env::default();
        env.mock_all_auths();
        let (ta, _tb, tc, router_addr) = setup_chain(&env);

        env.ledger().with_mut(|l| l.timestamp = 1_000);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);

        RouterClient::new(&env, &router_addr).swap_through(
            &trader,
            &ta,
            &tc,
            &100_000_i128,
            &0_i128,
            &500_u64,
        );
    }

    #[test]
    fn test_intermediate_balance_is_zero_after_swap() {
        let env = Env::default();
        env.mock_all_auths();
        let (ta, tb, tc, router_addr) = setup_chain(&env);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &50_000_i128);

        let deadline = env.ledger().timestamp() + 100;
        RouterClient::new(&env, &router_addr).swap_through(
            &trader,
            &ta,
            &tc,
            &50_000_i128,
            &0_i128,
            &deadline,
        );

        assert_eq!(StellarTokenClient::new(&env, &tb).balance(&trader), 0);
    }
}
