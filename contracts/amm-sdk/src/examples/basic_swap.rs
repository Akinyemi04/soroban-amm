//! Example: quote and execute a swap using the Rust SDK.
//!
//! Run with:
//! ```
//! cargo test --package soroban-amm-sdk --features testutils -- examples::basic_swap
//! ```

#[cfg(all(test, feature = "testutils"))]
mod basic_swap {
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        token::StellarAssetClient,
        Address, Bytes, Env,
    };

    use soroban_amm_sdk::client::AmmPoolSdk;

    /// Demonstrates:
    /// 1. Binding the SDK to a deployed pool.
    /// 2. Quoting a swap-in with price-impact validation.
    /// 3. Submitting the swap with the quoted min-out as slippage guard.
    #[test]
    fn swap_with_price_impact_check() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1_000_000);

        // — Setup pool (re-uses the AMM test helpers) ——————————————————————————
        let admin = Address::generate(&env);
        let amm_id = env.register_contract(None, amm::AmmPool);
        let lp_id = env.register_contract(None, token::LpToken);

        token::LpTokenClient::new(&env, &lp_id).initialize(
            &amm_id,
            &soroban_sdk::String::from_str(&env, "LP"),
            &soroban_sdk::String::from_str(&env, "LP"),
            &7u32,
        );

        let register_sac = |admin: &Address| {
            let c = env.register_stellar_asset_contract_v2(admin.clone());
            (
                soroban_sdk::token::TokenClient::new(&env, &c.address()),
                StellarAssetClient::new(&env, &c.address()),
            )
        };

        let (ta, ta_sac) = register_sac(&admin);
        let (tb, tb_sac) = register_sac(&admin);

        amm::AmmPoolClient::new(&env, &amm_id).initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_id,
            &30_i128, // 0.30% fee
            &admin,
            &0_i128,
        );

        // — Seed liquidity ————————————————————————————————————————————————————
        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &10_000_000_i128);
        tb_sac.mint(&provider, &10_000_000_i128);

        let sdk = AmmPoolSdk::new(&env, &amm_id);
        sdk.add_liquidity(&provider, 10_000_000, 10_000_000, 0, u64::MAX)
            .unwrap();

        // — Quote a swap-in ———————————————————————————————————————————————————
        let amount_in = 500_000_i128;
        let quote = sdk.quote_swap_in(&ta.address, amount_in).unwrap();

        assert!(quote.valid, "quote must be valid for a seeded pool");
        assert!(quote.amount_out > 0);
        assert!(
            quote.price_impact_bps < 1_000,
            "price impact should be < 10% for small trade"
        );

        // — Execute swap with 1% slippage tolerance ——————————————————————————
        let min_out = quote.amount_out * 99 / 100;
        let trader = Address::generate(&env);
        ta_sac.mint(&trader, &amount_in);

        let actual_out = sdk
            .execute_swap(&trader, &ta.address, amount_in, min_out, u64::MAX, None)
            .unwrap();

        assert_eq!(actual_out, quote.amount_out, "actual output matches quote");
    }

    /// Demonstrates the flash loan SDK wrapper and verifies the reentrancy
    /// guard is accessible via `flash_loan_locked`.
    #[test]
    fn flash_loan_locked_is_false_when_idle() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let amm_id = env.register_contract(None, amm::AmmPool);
        let lp_id = env.register_contract(None, token::LpToken);
        token::LpTokenClient::new(&env, &lp_id).initialize(
            &amm_id,
            &soroban_sdk::String::from_str(&env, "LP"),
            &soroban_sdk::String::from_str(&env, "LP"),
            &7u32,
        );

        let c = env.register_stellar_asset_contract_v2(admin.clone());
        let ta = soroban_sdk::token::TokenClient::new(&env, &c.address());
        let tc = env.register_stellar_asset_contract_v2(admin.clone());
        let tb = soroban_sdk::token::TokenClient::new(&env, &tc.address());

        amm::AmmPoolClient::new(&env, &amm_id).initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_id,
            &9_i128,
            &admin,
            &0_i128,
        );

        let sdk = AmmPoolSdk::new(&env, &amm_id);
        assert!(!sdk.flash_loan_locked(), "lock should be false when idle");
    }
}
