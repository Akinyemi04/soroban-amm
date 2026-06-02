//! Upgrade integration tests for AMM and factory contracts.

use amm::{AmmPool, AmmPoolClient, WASM as AMM_WASM};
use factory::{Factory, FactoryClient};
use soroban_sdk::{
    testutils::Address as _, token::StellarAssetClient, Address, BytesN, Env, String,
};
use token::{LpToken, LpTokenClient, WASM as TOKEN_WASM};

fn setup_amm(env: &Env) -> (AmmPoolClient<'_>, Address, Address, Address, Address) {
    env.budget().reset_unlimited();
    env.mock_all_auths();

    let admin = Address::generate(env);
    let token_a = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let token_b = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let lp_addr = env.register_contract(None, LpToken);
    let amm_addr = env.register_contract(None, AmmPool);

    LpTokenClient::new(env, &lp_addr).initialize(
        &amm_addr,
        &String::from_str(env, "LP"),
        &String::from_str(env, "LP"),
        &7,
    );

    let amm = AmmPoolClient::new(env, &amm_addr);
    amm.initialize(&admin, &token_a, &token_b, &lp_addr, &30, &admin, &0);

    (amm, amm_addr, admin, token_a, token_b)
}

#[test]
fn amm_upgrade_preserves_pool_state() {
    let env = Env::default();
    let (amm, amm_addr, _admin, token_a, token_b) = setup_amm(&env);

    let provider = Address::generate(&env);
    StellarAssetClient::new(&env, &token_a).mint(&provider, &1_000_000);
    StellarAssetClient::new(&env, &token_b).mint(&provider, &1_000_000);
    amm.add_liquidity(&provider, &500_000, &500_000, &0, &u64::MAX);

    let before = amm.get_info();
    let new_hash: BytesN<32> = env.deployer().upload_contract_wasm(AMM_WASM);
    amm.upgrade(&new_hash);

    let upgraded = AmmPoolClient::new(&env, &amm_addr);
    let after = upgraded.get_info();
    assert_eq!(after.reserve_a, before.reserve_a);
    assert_eq!(after.reserve_b, before.reserve_b);
    assert_eq!(after.total_shares, before.total_shares);
    assert_eq!(after.admin, before.admin);
}

#[test]
fn amm_upgrade_without_admin_auth_reverts() {
    let env = Env::default();
    let (amm, _, _, _, _) = setup_amm(&env);
    let new_hash: BytesN<32> = env.deployer().upload_contract_wasm(AMM_WASM);

    env.set_auths(&[]);
    let result = amm.try_upgrade(&new_hash);
    assert!(result.is_err(), "upgrade must require stored admin auth");
}

#[test]
fn factory_update_wasm_hashes_allows_new_pool_with_new_hashes() {
    let env = Env::default();
    env.budget().reset_unlimited();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let amm_hash: BytesN<32> = env.deployer().upload_contract_wasm(AMM_WASM);
    let token_hash: BytesN<32> = env.deployer().upload_contract_wasm(TOKEN_WASM);
    let new_amm_hash: BytesN<32> = env.deployer().upload_contract_wasm(AMM_WASM);
    let new_token_hash: BytesN<32> = env.deployer().upload_contract_wasm(TOKEN_WASM);

    let factory_addr = env.register_contract(None, Factory);
    let factory = FactoryClient::new(&env, &factory_addr);
    factory.initialize(&admin, &amm_hash, &token_hash);
    factory.update_wasm_hashes(&Some(new_amm_hash), &Some(new_token_hash));

    let token_a = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let token_b = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let (pool_addr, governance) =
        factory.create_pool_with_fee_bps(&admin, &token_a, &token_b, &30, &None);

    assert_eq!(governance, None);
    assert_eq!(
        factory.get_pool(&token_a, &token_b),
        Some(pool_addr.clone())
    );
    assert!(factory.get_lp_token(&pool_addr).is_some());
}
