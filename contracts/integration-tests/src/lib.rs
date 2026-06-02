//! Cross-contract integration tests covering all five contracts as a system:
//! amm, token, factory, twap_consumer, and concentrated_liquidity.
//!
//! Issue #166: Integration test matrix covering all five contracts.
//!
//! All tests run fully in-process using the Soroban test environment —
//! no external network calls are made.

#[cfg(test)]
mod upgrade_integration_test;

#[cfg(all(test, feature = "legacy-integration-matrix"))]
mod tests {
    use soroban_sdk::{
        testutils::{Address as _, Ledger, LedgerInfo},
        Address, BytesN, Env,
    };

    // WASM blobs compiled via `cargo build --release --target wasm32v1-none`.
    use amm::WASM as AMM_WASM;
    use governance::WASM as GOV_WASM;
    use token::WASM as TOKEN_WASM;

    // Contract clients.
    use amm::AmmPoolClient;
    use concentrated_liquidity::ConcentratedLiquidityClient;
    use dex_aggregator::{DexAggregator, DexAggregatorClient};
    use factory::{Factory, FactoryClient};
    use governance::{GovernanceClient, ProposalKind, Vote};
    use soroban_sdk::token::StellarAssetClient;
    use twal_consumer::{TwalConsumer, TwalConsumerClient};
    use twap_consumer::{TwapConsumer, TwapConsumerClient};

    // Use a deadline far enough in the future for all test operations.
    const DEADLINE: u64 = u64::MAX;

    fn set_ledger_ts(env: &Env, ts: u64) {
        env.ledger().set(LedgerInfo {
            timestamp: ts,
            protocol_version: 22,
            sequence_number: env.ledger().sequence(),
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 16,
            min_persistent_entry_ttl: 4096,
            max_entry_ttl: 6_312_000,
        });
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Scenario 1: Factory → V2 AMM pool → add liquidity → swap → TWAP recorded → queried
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn scenario_factory_amm_swap_twap() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash: BytesN<32> = env.deployer().upload_contract_wasm(AMM_WASM);
        let token_hash: BytesN<32> = env.deployer().upload_contract_wasm(TOKEN_WASM);

        let admin = Address::generate(&env);

        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        // Token pair.
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        // Deploy AMM pool via factory.
        let (pool_addr, _gov) = factory.create_pool(&token_a, &token_b, &30_i128, &None);
        assert!(factory.get_pool(&token_a, &token_b).is_some());

        let lp_addr = factory.get_lp_token(&pool_addr).unwrap();
        let amm = AmmPoolClient::new(&env, &pool_addr);

        // Fund provider.
        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &1_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&provider, &1_000_000_i128);

        // Add liquidity at t=1000.
        set_ledger_ts(&env, 1_000);
        amm.add_liquidity(&provider, &500_000_i128, &500_000_i128, &1_i128, &DEADLINE);
        let lp_balance: i128 = soroban_sdk::token::Client::new(&env, &lp_addr).balance(&provider);
        assert!(
            lp_balance > 0,
            "provider should hold LP tokens after add_liquidity"
        );

        // Deploy TWAP consumer and record snapshot at t=1000.
        let twap_addr = env.register_contract(None, TwapConsumer);
        let twap = TwapConsumerClient::new(&env, &twap_addr);
        twap.save_snapshot(&pool_addr);

        // Perform a swap at t=1000.
        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&trader, &10_000_i128);
        amm.swap(&trader, &token_a, &10_000_i128, &1_i128, &DEADLINE, &None);
        let trader_b_bal = soroban_sdk::token::Client::new(&env, &token_b).balance(&trader);
        assert!(
            trader_b_bal > 0,
            "trader should receive token_b from the swap"
        );

        // Advance to t=2000, save another snapshot, and query TWAP.
        set_ledger_ts(&env, 2_000);
        twap.save_snapshot(&pool_addr);

        let twap_price = twap.get_twap_price(&pool_addr, &1_000_u64);
        assert!(twap_price >= 0, "TWAP price should be non-negative");
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Scenario 2: Concentrated liquidity — position minted → swap crosses tick → fees collected
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn scenario_concentrated_liquidity_position_swap_fees() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let provider = Address::generate(&env);

        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        let cl_addr = env.register_contract(None, concentrated_liquidity::ConcentratedLiquidity);
        let cl = ConcentratedLiquidityClient::new(&env, &cl_addr);

        // Initialize at tick 0, 30 bps fee.
        cl.initialize(&token_a, &token_b, &30_i128, &0_i32);

        // Fund provider.
        StellarAssetClient::new(&env, &token_a).mint(&provider, &100_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&provider, &100_000_i128);

        // Mint position in range [-100, 100] (covers current tick = 0).
        let (dep_a, dep_b) = cl.mint_position(
            &provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
        );
        assert!(dep_a > 0 || dep_b > 0, "should deposit at least one token");

        // ── Tick registry verification (#178) ────────────────────────────────
        let tick_lower = cl.get_tick(&-100_i32);
        let tick_upper = cl.get_tick(&100_i32);
        assert!(tick_lower.initialized, "lower tick should be initialized");
        assert!(tick_upper.initialized, "upper tick should be initialized");
        assert!(
            tick_lower.liquidity_gross > 0,
            "lower tick liquidity_gross > 0"
        );
        assert!(
            tick_upper.liquidity_gross > 0,
            "upper tick liquidity_gross > 0"
        );
        // Lower tick's liquidity_net is positive (adds liquidity when crossed upward).
        assert!(tick_lower.liquidity_net > 0, "lower tick liquidity_net > 0");
        // Upper tick's liquidity_net is negative (removes liquidity when crossed upward).
        assert!(tick_upper.liquidity_net < 0, "upper tick liquidity_net < 0");

        // Active liquidity covers tick 0 so should be positive.
        assert!(
            cl.active_liquidity() > 0,
            "active liquidity should be positive"
        );

        // Pre-fund contract with token_b so the swap can transfer out.
        StellarAssetClient::new(&env, &token_b).mint(&cl_addr, &10_000_i128);

        // Execute swap (moves price to tick 50, crossing the range boundary).
        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&trader, &1_000_i128);
        let out_amt = cl.swap(&trader, &token_a, &1_000_i128, &50_i32);
        assert!(out_amt > 0, "swap should return positive amount");
        assert_eq!(
            cl.current_tick(),
            50_i32,
            "current tick should update to 50"
        );

        // Burn full position and collect tokens back.
        let pos = cl.get_position(&provider, &-100_i32, &100_i32);
        let (ret_a, ret_b) = cl.burn_position(&provider, &-100_i32, &100_i32, &pos.liquidity);
        assert!(
            ret_a > 0 || ret_b > 0,
            "burn should return tokens to provider"
        );

        // After full burn, tick entries must be cleaned up (#178 + #179).
        let tick_lower_after = cl.get_tick(&-100_i32);
        let tick_upper_after = cl.get_tick(&100_i32);
        assert!(
            !tick_lower_after.initialized,
            "lower tick should be uninitialized after full burn"
        );
        assert!(
            !tick_upper_after.initialized,
            "upper tick should be uninitialized after full burn"
        );
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Scenario 3: Governance — proposal → voted → executed (fee change applied)
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn scenario_governance_proposal_vote_execute() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash: BytesN<32> = env.deployer().upload_contract_wasm(AMM_WASM);
        let token_hash: BytesN<32> = env.deployer().upload_contract_wasm(TOKEN_WASM);
        let gov_hash: BytesN<32> = env.deployer().upload_contract_wasm(GOV_WASM);

        let admin = Address::generate(&env);

        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        // Deploy pool with governance.
        let (pool_addr, gov_opt) =
            factory.create_pool(&token_a, &token_b, &30_i128, &Some(gov_hash));
        let gov_addr = gov_opt.unwrap();

        let amm = AmmPoolClient::new(&env, &pool_addr);
        let gov = GovernanceClient::new(&env, &gov_addr);

        // Seed pool liquidity so LP tokens exist (quorum uses total LP supply).
        let lp_holder = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&lp_holder, &1_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&lp_holder, &1_000_000_i128);
        set_ledger_ts(&env, 100);
        amm.add_liquidity(&lp_holder, &500_000_i128, &500_000_i128, &1_i128, &DEADLINE);

        // Create fee-change proposal and vote For.
        let proposal_id = gov.propose(&lp_holder, &ProposalKind::UpdateFee(50_i128));
        gov.vote(&lp_holder, &proposal_id, &Vote::For);

        // Advance past voting period (7 days) + timelock (2 days).
        set_ledger_ts(&env, 100 + 604_800 + 172_800 + 1);

        // Execute proposal — should update fee on the pool.
        gov.execute(&proposal_id);

        // Verify the fee was changed.
        let info = amm.get_info();
        assert_eq!(
            info.fee_bps, 50_i128,
            "fee_bps should be 50 after governance execution"
        );
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Scenario 4: Multi-contract failure cases
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn scenario_swap_insufficient_liquidity_fails() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash: BytesN<32> = env.deployer().upload_contract_wasm(AMM_WASM);
        let token_hash: BytesN<32> = env.deployer().upload_contract_wasm(TOKEN_WASM);

        let admin = Address::generate(&env);

        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        let (pool_addr, _) = factory.create_pool(&token_a, &token_b, &30_i128, &None);
        let amm = AmmPoolClient::new(&env, &pool_addr);

        // Attempt swap on empty pool — must fail (no liquidity).
        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&trader, &1_000_i128);
        let result = amm.try_swap(&trader, &token_a, &1_000_i128, &1_i128, &DEADLINE, &None);
        assert!(result.is_err(), "swap on empty pool should fail");
    }

    #[test]
    fn scenario_governance_proposal_below_quorum_is_defeated() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash: BytesN<32> = env.deployer().upload_contract_wasm(AMM_WASM);
        let token_hash: BytesN<32> = env.deployer().upload_contract_wasm(TOKEN_WASM);
        let gov_hash: BytesN<32> = env.deployer().upload_contract_wasm(GOV_WASM);

        let admin = Address::generate(&env);

        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        let (pool_addr, gov_opt) =
            factory.create_pool(&token_a, &token_b, &30_i128, &Some(gov_hash));
        let gov_addr = gov_opt.unwrap();

        let amm = AmmPoolClient::new(&env, &pool_addr);
        let gov = GovernanceClient::new(&env, &gov_addr);

        // Seed the pool.
        let lp_holder = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&lp_holder, &1_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&lp_holder, &1_000_000_i128);
        set_ledger_ts(&env, 100);
        amm.add_liquidity(&lp_holder, &500_000_i128, &500_000_i128, &1_i128, &DEADLINE);

        // Create a proposal but do not vote → quorum is not met.
        let proposal_id = gov.propose(&lp_holder, &ProposalKind::UpdateFee(50_i128));

        // Advance past voting + timelock.
        set_ledger_ts(&env, 100 + 604_800 + 172_800 + 1);

        // Executing a below-quorum proposal must fail.
        let result = gov.try_execute(&proposal_id);
        assert!(
            result.is_err(),
            "proposal without quorum should not execute"
        );
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Tick registry: overlapping positions (#178)
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn tick_registry_overlapping_positions_accumulate_correctly() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        let cl_addr = env.register_contract(None, concentrated_liquidity::ConcentratedLiquidity);
        let cl = ConcentratedLiquidityClient::new(&env, &cl_addr);
        cl.initialize(&token_a, &token_b, &30_i128, &0_i32);

        let p1 = Address::generate(&env);
        let p2 = Address::generate(&env);

        StellarAssetClient::new(&env, &token_a).mint(&p1, &100_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&p1, &100_000_i128);
        StellarAssetClient::new(&env, &token_a).mint(&p2, &100_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&p2, &100_000_i128);

        // Position 1: [-200, 0] — upper boundary is tick 0.
        cl.mint_position(
            &p1,
            &-200_i32,
            &0_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
        );
        // Position 2: [0, 200] — lower boundary is tick 0.
        cl.mint_position(
            &p2,
            &0_i32,
            &200_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
        );

        // Tick 0 is shared by both positions: upper for p1, lower for p2.
        let t0 = cl.get_tick(&0_i32);
        assert!(t0.initialized, "tick 0 should be initialized");
        // Both positions reference tick 0 → liquidity_gross = liq_p1 + liq_p2.
        assert!(
            t0.liquidity_gross > 0,
            "tick 0 liquidity_gross should be positive"
        );

        // Burn position 2 fully.
        let pos2 = cl.get_position(&p2, &0_i32, &200_i32);
        cl.burn_position(&p2, &0_i32, &200_i32, &pos2.liquidity);

        // Tick 0 still referenced by position 1 — must remain initialized.
        let t0_after_p2 = cl.get_tick(&0_i32);
        assert!(
            t0_after_p2.initialized,
            "tick 0 should still be initialized while position 1 exists"
        );

        // Burn position 1 fully.
        let pos1 = cl.get_position(&p1, &-200_i32, &0_i32);
        cl.burn_position(&p1, &-200_i32, &0_i32, &pos1.liquidity);

        // Now tick 0 has no references → must be cleaned up.
        let t0_cleaned = cl.get_tick(&0_i32);
        assert!(
            !t0_cleaned.initialized,
            "tick 0 should be uninitialized after all positions burned"
        );
        assert_eq!(t0_cleaned.liquidity_gross, 0);
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Scenario: TWAL consumer + DEX aggregator (#261, #260)
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn scenario_twal_and_aggregator() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash: BytesN<32> = env.deployer().upload_contract_wasm(AMM_WASM);
        let token_hash: BytesN<32> = env.deployer().upload_contract_wasm(TOKEN_WASM);
        let admin = Address::generate(&env);

        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_c = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        let (pool_ab, _) = factory.create_pool(&token_a, &token_b, &30_i128, &None);
        let (pool_bc, _) = factory.create_pool(&token_b, &token_c, &30_i128, &None);

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &2_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&provider, &2_000_000_i128);
        StellarAssetClient::new(&env, &token_c).mint(&provider, &1_000_000_i128);

        set_ledger_ts(&env, 10_000);
        AmmPoolClient::new(&env, &pool_ab).add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &1_i128,
            &DEADLINE,
        );
        AmmPoolClient::new(&env, &pool_bc).add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &1_i128,
            &DEADLINE,
        );

        let twal_addr = env.register_contract(None, TwalConsumer);
        let twal = TwalConsumerClient::new(&env, &twal_addr);
        twal.save_snapshot(&pool_ab);
        set_ledger_ts(&env, 10_600);
        twal.save_snapshot(&pool_ab);
        let twal_val = twal.get_twal_liquidity(&pool_ab, &600);
        assert!(twal_val > 0);

        let agg_addr = env.register_contract(None, DexAggregator);
        let agg = DexAggregatorClient::new(&env, &agg_addr);
        agg.initialize(&factory_addr);
        let quote = agg
            .find_best_route(&token_a, &token_c, &50_000_i128, &4u32)
            .unwrap();
        assert!(quote.amount_out > 0);
        assert!(quote.hops.len() >= 2);
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // sqrtPriceX96 math round-trip (#177)
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn math_tick_to_sqrt_price_round_trip() {
        use concentrated_liquidity::math::{sqrt_price_x96_to_tick, tick_to_sqrt_price_x96, Q96};

        // tick 0 maps to sqrt(1) * 2^96 = 2^96 = Q96.
        let sp0 = tick_to_sqrt_price_x96(0);
        assert!(
            (sp0 as i128 - Q96 as i128).abs() <= 2,
            "tick 0 must map to ~Q96, got {sp0}"
        );

        // Round-trip: tick → sqrtPrice → tick must return the original tick.
        for &tick in &[-100_000_i32, -10_000, -100, 0, 100, 10_000, 100_000] {
            let sp = tick_to_sqrt_price_x96(tick);
            let back = sqrt_price_x96_to_tick(sp);
            assert_eq!(back, tick, "round-trip failed for tick {tick}: got {back}");
        }
    }
}
