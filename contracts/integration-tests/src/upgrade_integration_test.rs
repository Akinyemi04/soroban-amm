//! Tests for contract upgrade flows.
//! Verifies that state persists across an upgrade of the AMM contract.

#[cfg(test)]
mod upgrade_tests {
    use super::*;
    use soroban_sdk::{Env, BytesN, Address, testutils::{Address as _, LedgerInfo}};
    use amm::{WASM as AMM_V1, WASM as AMM_V2};
    use token::WASM as TOKEN_WASM;
    use amm::AmmPoolClient;
    use soroban_sdk::token::StellarAssetClient;

    const DEADLINE: u64 = u64::MAX;

    #[test]
    fn amm_upgrade_preserves_state() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let token_a = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let token_b = env.register_stellar_asset_contract_v2(admin.clone()).address();
        // Deploy AMM V1
        let amm_addr = env.register_contract(&"amm_v1", amm::AmmPool);
        let amm = AmmPoolClient::new(&env, &amm_addr);
        // Assume initialize method exists for V1 (placeholder)
        // amm.initialize(&admin, &AMM_V1, &TOKEN_WASM).unwrap(); // not needed if V1 uses same init
        // Provide initial liquidity
        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &1_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&provider, &1_000_000_i128);
        // Add liquidity via AMM V1
        amm.add_liquidity(&provider, 500_000_i128, 500_000_i128, 1, DEADLINE).unwrap();
        let lp_before = amm.get_lp_balance(&provider);
        // Upgrade contract to V2
        env.upgrade_contract(&amm_addr, &AMM_V2).unwrap();
        // Re‑instantiate client after upgrade
        let amm_upgraded = AmmPoolClient::new(&env, &amm_addr);
        // State should be unchanged
        let lp_after = amm_upgraded.get_lp_balance(&provider);
        assert_eq!(lp_before, lp_after, "LP balance must persist after upgrade");
    }
}
