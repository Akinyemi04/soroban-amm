#![no_std]

use soroban_sdk::{contract, contractclient, contractimpl, contracttype, Address, Env, Vec};

#[contractclient(name = "AmmPoolOracleClient")]
pub trait AmmPoolOracle {
    fn get_price_cumulative(env: Env) -> (i128, i128, u64);
}

#[contractclient(name = "ClPoolOracleClient")]
pub trait ClPoolOracle {
    fn get_tick_cumulative(env: Env) -> (i64, u64);
}

#[contracttype]
pub enum DataKey {
    Snapshot(Address, u64),
    TrackedPools,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceSnapshot {
    pub cum_a: i128,
    pub cum_b: i128,
    pub pool_ts: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceValidation {
    pub spot_price: i128,
    pub twap_price: i128,
    pub deviation_bps: i128,
    pub max_deviation_bps: i128,
    pub is_deviation: bool,
}

#[contract]
pub struct TwapConsumer;

#[contractimpl]
impl TwapConsumer {
    /// Keep snapshot alive for 7 days (in ledgers: 7 * 24 * 3600 / 5 ≈ 120,960)
    pub const SNAPSHOT_TTL_LEDGERS: u32 = 120_960;
    pub const BPS_DENOMINATOR: i128 = 10_000;
    pub const PRICE_SCALE: i128 = 1_000_000;

    /// Stores a pool cumulative-price snapshot keyed by the pool timestamp.
    ///
    /// Also registers the pool in the TrackedPools list if it has not been seen before,
    /// enabling `get_tracked_pools` and `get_twap_all` to enumerate all observed pools.
    pub fn save_snapshot(env: Env, pool: Address) {
        let (cum_a, cum_b, pool_ts) = AmmPoolOracleClient::new(&env, &pool).get_price_cumulative();
        let ledger_ts = env.ledger().timestamp(); // key by keeper clock, not pool clock
        let snapshot = PriceSnapshot {
            cum_a,
            cum_b,
            pool_ts,
        };
        let key = DataKey::Snapshot(pool.clone(), ledger_ts);
        env.storage().persistent().set(&key, &snapshot);
        env.storage().persistent().extend_ttl(
            &key,
            Self::SNAPSHOT_TTL_LEDGERS / 2,
            Self::SNAPSHOT_TTL_LEDGERS,
        );

        // Register pool if not already tracked.
        let mut tracked: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::TrackedPools)
            .unwrap_or_else(|| Vec::new(&env));
        let mut already_tracked = false;
        for i in 0..tracked.len() {
            if tracked.get(i).unwrap() == pool {
                already_tracked = true;
                break;
            }
        }
        if !already_tracked {
            tracked.push_back(pool);
            env.storage()
                .instance()
                .set(&DataKey::TrackedPools, &tracked);
        }
    }

    /// Deletes a price snapshot from persistent storage.
    pub fn delete_snapshot(env: Env, pool: Address, ledger_ts: u64) {
        let key = DataKey::Snapshot(pool, ledger_ts);
        env.storage().persistent().remove(&key);
    }

    /// Computes TWAP for token A in terms of token B over `window_seconds`.
    ///
    /// Returns: (cum_a_now - cum_a_then) / window_seconds.
    pub fn get_twap_price(env: Env, pool: Address, window_seconds: u64) -> i128 {
        assert!(window_seconds > 0, "window_seconds must be > 0");

        let (cum_a_now, _cum_b_now, pool_ts_now) =
            AmmPoolOracleClient::new(&env, &pool).get_price_cumulative();
        let ledger_ts_now = env.ledger().timestamp();
        assert!(
            ledger_ts_now >= window_seconds,
            "ledger timestamp is smaller than requested window"
        );

        let then_ts = ledger_ts_now - window_seconds;
        let snapshot: PriceSnapshot = env
            .storage()
            .persistent()
            .get(&DataKey::Snapshot(pool.clone(), then_ts))
            .unwrap_or_else(|| panic!("missing snapshot at target ledger timestamp {then_ts}"));

        // Use wrapping_sub on u128 to handle accumulator wrap-around correctly.
        let delta_a = (cum_a_now as u128).wrapping_sub(snapshot.cum_a as u128) as i128;
        let elapsed = (pool_ts_now - snapshot.pool_ts) as i128;
        assert!(elapsed > 0, "window too small (pool time did not advance)");

        delta_a / elapsed
    }

    /// Validates a real-time price against a TWAP price.
    ///
    /// Prices must use the AMM scale factor of 1_000_000. The deviation threshold
    /// is expressed in basis points where 100 = 1%.
    pub fn validate_price(
        spot_price: i128,
        twap_price: i128,
        max_deviation_bps: i128,
    ) -> PriceValidation {
        assert!(spot_price > 0, "spot_price must be > 0");
        assert!(twap_price > 0, "twap_price must be > 0");
        assert!(
            (0..=Self::BPS_DENOMINATOR).contains(&max_deviation_bps),
            "max_deviation_bps must be between 0 and 10000"
        );

        let price_delta = if spot_price >= twap_price {
            spot_price - twap_price
        } else {
            twap_price - spot_price
        };
        let deviation_bps = price_delta * Self::BPS_DENOMINATOR / twap_price;

        PriceValidation {
            spot_price,
            twap_price,
            deviation_bps,
            max_deviation_bps,
            is_deviation: deviation_bps > max_deviation_bps,
        }
    }

    /// Computes the pool TWAP, compares it with the caller-supplied real-time
    /// AMM spot price, and flags prices outside the configured threshold.
    pub fn validate_price_against_twap(
        env: Env,
        pool: Address,
        window_seconds: u64,
        spot_price: i128,
        max_deviation_bps: i128,
    ) -> PriceValidation {
        let twap_price = Self::get_twap_price(env, pool, window_seconds);
        Self::validate_price(spot_price, twap_price, max_deviation_bps)
    }

    /// Lending integration helper: returns the validated collateral value when
    /// the real-time price is within the TWAP deviation threshold and panics
    /// when the spot price is likely manipulated.
    pub fn assert_lending_price_safe(
        env: Env,
        pool: Address,
        window_seconds: u64,
        spot_price: i128,
        max_deviation_bps: i128,
        collateral_amount: i128,
    ) -> i128 {
        assert!(collateral_amount >= 0, "collateral_amount must be >= 0");
        let validation = Self::validate_price_against_twap(
            env,
            pool,
            window_seconds,
            spot_price,
            max_deviation_bps,
        );
        assert!(
            !validation.is_deviation,
            "spot price exceeds TWAP deviation threshold"
        );

        collateral_amount * validation.spot_price / Self::PRICE_SCALE
    }

    /// Computes TWAP for both token directions using tick accumulator approach.
    ///
    /// Returns (twap_a_to_b, twap_b_to_a) where:
    /// - twap_a_to_b: average price of B in terms of A (amount of B per unit A)
    /// - twap_b_to_a: average price of A in terms of B (amount of A per unit B)
    ///
    /// This follows the V3 pattern of deriving TWAP from cumulative accumulators over time.
    pub fn get_twap_both(env: Env, pool: Address, window_seconds: u64) -> (i128, i128) {
        assert!(window_seconds > 0, "window_seconds must be > 0");

        let (cum_a_now, cum_b_now, pool_ts_now) =
            AmmPoolOracleClient::new(&env, &pool).get_price_cumulative();
        let ledger_ts_now = env.ledger().timestamp();
        assert!(
            ledger_ts_now >= window_seconds,
            "ledger timestamp is smaller than requested window"
        );

        let then_ts = ledger_ts_now - window_seconds;
        let snapshot: PriceSnapshot = env
            .storage()
            .persistent()
            .get(&DataKey::Snapshot(pool.clone(), then_ts))
            .unwrap_or_else(|| panic!("missing snapshot at target ledger timestamp {then_ts}"));

        // Use wrapping_sub on u128 to handle accumulator wrap-around correctly.
        let delta_a = (cum_a_now as u128).wrapping_sub(snapshot.cum_a as u128) as i128;
        let delta_b = (cum_b_now as u128).wrapping_sub(snapshot.cum_b as u128) as i128;
        let elapsed = (pool_ts_now - snapshot.pool_ts) as i128;
        assert!(elapsed > 0, "window too small (pool time did not advance)");

        // Average tick-like accumulators over the elapsed time
        let twap_a_to_b = delta_a / elapsed;
        let twap_b_to_a = delta_b / elapsed;

        (twap_a_to_b, twap_b_to_a)
    }

    /// Returns the list of pool addresses that have had at least one snapshot saved.
    pub fn get_tracked_pools(env: Env) -> Vec<Address> {
        env.storage()
            .instance()
            .get(&DataKey::TrackedPools)
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Returns the TWAP for every tracked AMM pool in one call.
    ///
    /// Each element is `(pool_address, twap_price)` where `twap_price` is the
    /// time-weighted average price of token A denominated in token B, scaled by
    /// 1_000_000, over `window_seconds`.
    ///
    /// A snapshot at `now - window_seconds` must have been saved for every
    /// tracked pool, otherwise this call panics.
    pub fn get_twap_all(env: Env, window_seconds: u64) -> Vec<(Address, i128)> {
        let tracked: Vec<Address> = Self::get_tracked_pools(env.clone());
        let mut results: Vec<(Address, i128)> = Vec::new(&env);
        for i in 0..tracked.len() {
            let pool = tracked.get(i).unwrap();
            let twap = Self::get_twap_price(env.clone(), pool.clone(), window_seconds);
            results.push_back((pool, twap));
        }
        results
    }

    /// Computes the time-weighted average tick from a CL pool over `window_seconds`.
    ///
    /// Uses `get_tick_cumulative` from the CL pool. Returns the average tick value,
    /// which maps to a price via 1.0001^avg_tick. A snapshot at `now - window_seconds`
    /// must have been saved previously via `save_snapshot`.
    pub fn get_cl_twap(env: Env, pool: Address, window_seconds: u64) -> i64 {
        assert!(window_seconds > 0, "window_seconds must be > 0");

        let (cum_now, last_ts_now) = ClPoolOracleClient::new(&env, &pool).get_tick_cumulative();
        let ledger_ts_now = env.ledger().timestamp();
        assert!(
            ledger_ts_now >= window_seconds,
            "ledger timestamp is smaller than requested window"
        );

        let then_ts = ledger_ts_now - window_seconds;
        let snapshot: PriceSnapshot = env
            .storage()
            .persistent()
            .get(&DataKey::Snapshot(pool.clone(), then_ts))
            .unwrap_or_else(|| panic!("missing snapshot at target ledger timestamp {then_ts}"));

        // snapshot.cum_a stores the tick_cumulative cast to i128 for CL pools.
        let cum_then = snapshot.cum_a as i64;
        let elapsed_pool = (last_ts_now - snapshot.pool_ts) as i64;
        assert!(
            elapsed_pool > 0,
            "window too small (pool time did not advance)"
        );

        ((cum_now - cum_then) / elapsed_pool) as i64
    }

    /// Save a snapshot from a CL pool (stores tick_cumulative in the cum_a field).
    pub fn save_cl_snapshot(env: Env, pool: Address) {
        let (tick_cum, pool_ts) = ClPoolOracleClient::new(&env, &pool).get_tick_cumulative();
        let ledger_ts = env.ledger().timestamp();
        let snapshot = PriceSnapshot {
            cum_a: tick_cum as i128,
            cum_b: 0,
            pool_ts,
        };
        let key = DataKey::Snapshot(pool, ledger_ts);
        env.storage().persistent().set(&key, &snapshot);
        env.storage().persistent().extend_ttl(
            &key,
            Self::SNAPSHOT_TTL_LEDGERS / 2,
            Self::SNAPSHOT_TTL_LEDGERS,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use amm::{AmmPool, AmmPoolClient};
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        token::{StellarAssetClient, TokenClient as StellarTokenClient},
        Address, Env,
    };
    use token::LpToken;

    /// Register a Stellar Asset Contract and return (TokenClient, StellarAssetClient).
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

    #[test]
    fn test_get_twap_price_diverges_from_spot_after_large_trade() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        let consumer_addr = env.register_contract(None, TwapConsumer);

        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "AMM LP Token"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );

        let (ta, ta_sac) = create_sac(&env, &admin);
        let (tb, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &2_000_000_i128);
        tb_sac.mint(&provider, &2_000_000_i128);
        amm.add_liquidity(
            &provider,
            &2_000_000_i128,
            &2_000_000_i128,
            &0_i128,
            &10_000_u64,
        );

        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.save_snapshot(&amm_addr);

        // Let 60s pass at the pre-trade price, then execute a large trade that moves spot.
        env.ledger().set_timestamp(10_060);
        let whale = Address::generate(&env);
        ta_sac.mint(&whale, &1_000_000_i128);
        amm.swap(
            &whale,
            &ta.address,
            &1_000_000_i128,
            &0_i128,
            &10_060_u64,
            &None,
        );

        let twap = consumer.get_twap_price(&amm_addr, &60_u64);
        let (spot_a, _spot_b) = amm.price_ratio();

        assert_eq!(twap, 1_000_000);
        assert!(twap > spot_a);
        assert_ne!(twap, spot_a);
    }

    #[test]
    fn test_validate_price_against_twap_flags_large_deviation() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        let consumer_addr = env.register_contract(None, TwapConsumer);

        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "AMM LP Token"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );

        let (ta, ta_sac) = create_sac(&env, &admin);
        let (tb, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &2_000_000_i128);
        tb_sac.mint(&provider, &2_000_000_i128);
        amm.add_liquidity(
            &provider,
            &2_000_000_i128,
            &2_000_000_i128,
            &0_i128,
            &10_000_u64,
        );

        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.save_snapshot(&amm_addr);

        env.ledger().set_timestamp(10_060);
        let whale = Address::generate(&env);
        ta_sac.mint(&whale, &1_000_000_i128);
        amm.swap(
            &whale,
            &ta.address,
            &1_000_000_i128,
            &0_i128,
            &10_060_u64,
            &None,
        );

        let (spot_a, _spot_b) = amm.price_ratio();
        let validation =
            consumer.validate_price_against_twap(&amm_addr, &60_u64, &spot_a, &500_i128);

        assert_eq!(validation.twap_price, 1_000_000);
        assert_eq!(validation.max_deviation_bps, 500);
        assert!(validation.deviation_bps > 500);
        assert!(validation.is_deviation);
    }

    #[test]
    fn test_lending_helper_accepts_safe_price_and_rejects_manipulated_price() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        let consumer_addr = env.register_contract(None, TwapConsumer);

        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "AMM LP Token"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );

        let (ta, ta_sac) = create_sac(&env, &admin);
        let (tb, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &2_000_000_i128);
        tb_sac.mint(&provider, &2_000_000_i128);
        amm.add_liquidity(
            &provider,
            &2_000_000_i128,
            &2_000_000_i128,
            &0_i128,
            &10_000_u64,
        );

        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.save_snapshot(&amm_addr);

        env.ledger().set_timestamp(10_060);
        let trader = Address::generate(&env);
        ta_sac.mint(&trader, &1_000_i128);
        amm.swap(
            &trader,
            &ta.address,
            &1_000_i128,
            &0_i128,
            &10_060_u64,
            &None,
        );

        let (safe_spot, _spot_b) = amm.price_ratio();
        let collateral_value = consumer.assert_lending_price_safe(
            &amm_addr,
            &60_u64,
            &safe_spot,
            &500_i128,
            &3_000_000_i128,
        );
        assert!(collateral_value > 0);

        let manipulated_spot = 600_000_i128;
        let result = consumer.try_assert_lending_price_safe(
            &amm_addr,
            &60_u64,
            &manipulated_spot,
            &500_i128,
            &3_000_000_i128,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_get_twap_both() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        let consumer_addr = env.register_contract(None, TwapConsumer);

        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "AMM LP Token"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );

        let (ta, ta_sac) = create_sac(&env, &admin);
        let (tb, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &2_000_000_i128);
        tb_sac.mint(&provider, &2_000_000_i128);
        amm.add_liquidity(
            &provider,
            &2_000_000_i128,
            &2_000_000_i128,
            &0_i128,
            &10_000_u64,
        );

        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.save_snapshot(&amm_addr);

        // Let 60s pass
        env.ledger().set_timestamp(10_060);
        let whale = Address::generate(&env);
        ta_sac.mint(&whale, &1_000_i128);
        amm.swap(
            &whale,
            &ta.address,
            &1_000_i128,
            &0_i128,
            &10_060_u64,
            &None,
        );

        let (twap_a_to_b, twap_b_to_a) = consumer.get_twap_both(&amm_addr, &60_u64);

        // With equal initial reserves, TWAPs should be reciprocals
        assert_eq!(twap_a_to_b, 1_000_000);
        assert_eq!(twap_b_to_a, 1_000_000);
    }

    #[test]
    fn test_get_twap_both_with_imbalance() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        let consumer_addr = env.register_contract(None, TwapConsumer);

        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "AMM LP Token"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );

        let (ta, ta_sac) = create_sac(&env, &admin);
        let (tb, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &2_000_000_i128);
        tb_sac.mint(&provider, &4_000_000_i128);
        amm.add_liquidity(
            &provider,
            &2_000_000_i128,
            &4_000_000_i128,
            &0_i128,
            &10_000_u64,
        );

        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.save_snapshot(&amm_addr);

        // Let 60s pass
        env.ledger().set_timestamp(10_060);
        let whale = Address::generate(&env);
        ta_sac.mint(&whale, &1_000_i128);
        amm.swap(
            &whale,
            &ta.address,
            &1_000_i128,
            &0_i128,
            &10_060_u64,
            &None,
        );

        let (twap_a_to_b, twap_b_to_a) = consumer.get_twap_both(&amm_addr, &60_u64);

        // With 1:2 reserves, twap_a_to_b should be 2M and twap_b_to_a should be 0.5M
        assert_eq!(twap_a_to_b, 2_000_000);
        assert_eq!(twap_b_to_a, 500_000);
    }

    // Issue #200: save_snapshot called for two pools — both appear in get_tracked_pools,
    // and get_twap_all returns correct TWAPs for both.
    #[test]
    fn test_get_tracked_pools_and_twap_all() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);

        // ── Pool 1 setup ──────────────────────────────────────────────────────
        let amm_addr1 = env.register_contract(None, AmmPool);
        let lp_addr1 = env.register_contract(None, LpToken);
        token::LpTokenClient::new(&env, &lp_addr1).initialize(
            &amm_addr1,
            &soroban_sdk::String::from_str(&env, "LP1"),
            &soroban_sdk::String::from_str(&env, "LP1"),
            &7u32,
        );
        let (ta1, ta1_sac) = create_sac(&env, &admin);
        let (tb1, tb1_sac) = create_sac(&env, &admin);
        let amm1 = AmmPoolClient::new(&env, &amm_addr1);
        amm1.initialize(
            &admin,
            &ta1.address,
            &tb1.address,
            &lp_addr1,
            &30_i128,
            &admin,
            &0_i128,
        );
        let p1 = Address::generate(&env);
        ta1_sac.mint(&p1, &2_000_000_i128);
        tb1_sac.mint(&p1, &2_000_000_i128);
        amm1.add_liquidity(&p1, &2_000_000_i128, &2_000_000_i128, &0_i128, &10_000_u64);

        // ── Pool 2 setup (2:1 reserve ratio) ─────────────────────────────────
        let amm_addr2 = env.register_contract(None, AmmPool);
        let lp_addr2 = env.register_contract(None, LpToken);
        token::LpTokenClient::new(&env, &lp_addr2).initialize(
            &amm_addr2,
            &soroban_sdk::String::from_str(&env, "LP2"),
            &soroban_sdk::String::from_str(&env, "LP2"),
            &7u32,
        );
        let (ta2, ta2_sac) = create_sac(&env, &admin);
        let (tb2, tb2_sac) = create_sac(&env, &admin);
        let amm2 = AmmPoolClient::new(&env, &amm_addr2);
        amm2.initialize(
            &admin,
            &ta2.address,
            &tb2.address,
            &lp_addr2,
            &30_i128,
            &admin,
            &0_i128,
        );
        let p2 = Address::generate(&env);
        ta2_sac.mint(&p2, &2_000_000_i128);
        tb2_sac.mint(&p2, &4_000_000_i128);
        amm2.add_liquidity(&p2, &2_000_000_i128, &4_000_000_i128, &0_i128, &10_000_u64);

        let consumer_addr = env.register_contract(None, TwapConsumer);
        let consumer = TwapConsumerClient::new(&env, &consumer_addr);

        // Save a snapshot for each pool at t=10_000.
        consumer.save_snapshot(&amm_addr1);
        consumer.save_snapshot(&amm_addr2);

        // Both pools must appear in get_tracked_pools.
        let tracked = consumer.get_tracked_pools();
        assert_eq!(tracked.len(), 2);
        assert!(tracked.contains(&amm_addr1));
        assert!(tracked.contains(&amm_addr2));

        // Calling save_snapshot again for pool1 must NOT add a duplicate.
        consumer.save_snapshot(&amm_addr1);
        assert_eq!(consumer.get_tracked_pools().len(), 2);

        // Advance time and do small swaps to bump the price accumulators.
        env.ledger().set_timestamp(10_060);
        let whale1 = Address::generate(&env);
        ta1_sac.mint(&whale1, &1_000_i128);
        amm1.swap(
            &whale1,
            &ta1.address,
            &1_000_i128,
            &0_i128,
            &10_060_u64,
            &None,
        );
        let whale2 = Address::generate(&env);
        ta2_sac.mint(&whale2, &1_000_i128);
        amm2.swap(
            &whale2,
            &ta2.address,
            &1_000_i128,
            &0_i128,
            &10_060_u64,
            &None,
        );

        // get_twap_all must return TWAPs for both pools.
        let all_twaps = consumer.get_twap_all(&60_u64);
        assert_eq!(all_twaps.len(), 2);

        // Pool 1 (1:1 reserves) → TWAP ≈ 1_000_000.
        let twap1 = consumer.get_twap_price(&amm_addr1, &60_u64);
        assert_eq!(twap1, 1_000_000);

        // Pool 2 (1:2 reserves) → TWAP ≈ 2_000_000.
        let twap2 = consumer.get_twap_price(&amm_addr2, &60_u64);
        assert_eq!(twap2, 2_000_000);

        // Verify get_twap_all returns correct values for each pool.
        for i in 0..all_twaps.len() {
            let (pool, twap) = all_twaps.get(i).unwrap();
            if pool == amm_addr1 {
                assert_eq!(twap, twap1);
            } else {
                assert_eq!(twap, twap2);
            }
        }
    }

    #[test]
    fn test_delete_snapshot() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        let consumer_addr = env.register_contract(None, TwapConsumer);

        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "AMM LP Token"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );

        let (ta, ta_sac) = create_sac(&env, &admin);
        let (tb, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &2_000_000_i128);
        tb_sac.mint(&provider, &2_000_000_i128);
        amm.add_liquidity(
            &provider,
            &2_000_000_i128,
            &2_000_000_i128,
            &0_i128,
            &10_000_u64,
        );

        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.save_snapshot(&amm_addr);

        // Verify snapshot was saved and can be used (get_twap_price does not panic)
        env.ledger().set_timestamp(10_060);
        let whale = Address::generate(&env);
        ta_sac.mint(&whale, &1_000_i128);
        amm.swap(
            &whale,
            &ta.address,
            &1_000_i128,
            &0_i128,
            &10_060_u64,
            &None,
        );
        let price = consumer.get_twap_price(&amm_addr, &60_u64);
        assert_eq!(price, 1_000_000);

        // Delete the snapshot at timestamp 10_000
        consumer.delete_snapshot(&amm_addr, &10_000);

        // Verify that calling get_twap_price now panics (since target snapshot is missing)
        let result = consumer.try_get_twap_price(&amm_addr, &60_u64);
        assert!(result.is_err());
    }
}
