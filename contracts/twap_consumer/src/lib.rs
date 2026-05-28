#![no_std]

use soroban_sdk::{contract, contractclient, contractimpl, contracttype, Address, Env};

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
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceSnapshot {
    pub cum_a: i128,
    pub cum_b: i128,
    pub pool_ts: u64,
}

#[contract]
pub struct TwapConsumer;

#[contractimpl]
impl TwapConsumer {
    /// Keep snapshot alive for 7 days (in ledgers: 7 * 24 * 3600 / 5 ≈ 120,960)
    pub const SNAPSHOT_TTL_LEDGERS: u32 = 120_960;

    /// Stores a pool cumulative-price snapshot keyed by the pool timestamp.
    pub fn save_snapshot(env: Env, pool: Address) {
        let (cum_a, cum_b, pool_ts) = AmmPoolOracleClient::new(&env, &pool).get_price_cumulative();
        let ledger_ts = env.ledger().timestamp(); // key by keeper clock, not pool clock
        let snapshot = PriceSnapshot {
            cum_a,
            cum_b,
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
        assert!(elapsed_pool > 0, "window too small (pool time did not advance)");

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
        env.storage()
            .persistent()
            .extend_ttl(&key, Self::SNAPSHOT_TTL_LEDGERS / 2, Self::SNAPSHOT_TTL_LEDGERS);
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
