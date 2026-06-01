#![cfg(test)]

extern crate std;

use soroban_sdk::{
    contract, contractimpl,
    testutils::{Address as _, Ledger, LedgerInfo},
    Address, Env,
};

use super::*;

// ── Mock adapter ────────────────────────────────────────────────────────────
//
// A configurable adapter contract used as the registered source in
// every test. `quote()` reads a per-instance `price` from storage so
// individual tests can dial each source independently.

#[contract]
struct MockAdapter;

const PRICE_KEY: &str = "price";

#[contractimpl]
impl MockAdapter {
    pub fn set_price(env: Env, price: i128) {
        env.storage()
            .instance()
            .set(&soroban_sdk::symbol_short!("price"), &price);
    }

    pub fn quote(env: Env, _token_a: Address, _token_b: Address) -> i128 {
        env.storage()
            .instance()
            .get(&soroban_sdk::symbol_short!("price"))
            .unwrap_or(0)
    }
}

struct Harness<'a> {
    env: Env,
    aggregator: OracleAggregatorClient<'a>,
    admin: Address,
    token_a: Address,
    token_b: Address,
}

fn deploy(env: &Env, max_staleness: u64) -> Harness<'_> {
    env.mock_all_auths();
    let admin = Address::generate(env);
    let aggregator_id = env.register_contract(None, OracleAggregator);
    let aggregator = OracleAggregatorClient::new(env, &aggregator_id);
    aggregator.initialize(&admin, &max_staleness);
    let token_a = Address::generate(env);
    let token_b = Address::generate(env);
    Harness {
        env: env.clone(),
        aggregator,
        admin,
        token_a,
        token_b,
    }
}

fn deploy_source(env: &Env, price: i128) -> Address {
    let id = env.register_contract(None, MockAdapter);
    let client = MockAdapterClient::new(env, &id);
    client.set_price(&price);
    id
}

fn set_now(env: &Env, ts: u64) {
    env.ledger().set(LedgerInfo {
        timestamp: ts,
        protocol_version: 22,
        sequence_number: 0,
        network_id: Default::default(),
        base_reserve: 10,
        min_temp_entry_ttl: 1,
        min_persistent_entry_ttl: 1,
        max_entry_ttl: 6_312_000,
    });
}

#[test]
fn initialize_seeds_admin_and_staleness() {
    let env = Env::default();
    let h = deploy(&env, 600);
    assert_eq!(h.aggregator.get_admin(), h.admin);
    assert_eq!(h.aggregator.get_max_staleness(), 600);
    assert_eq!(h.aggregator.list_sources().len(), 0);
}

#[test]
#[should_panic(expected = "Error(Contract, #1)")]
fn initialize_is_one_time() {
    let env = Env::default();
    let h = deploy(&env, 600);
    h.aggregator.initialize(&h.admin, &600);
}

#[test]
fn register_source_appends_to_registry() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 110);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::External);
    let sources = h.aggregator.list_sources();
    assert_eq!(sources.len(), 2);
    assert_eq!(sources.get_unchecked(0).source_contract, s1);
    assert_eq!(sources.get_unchecked(1).source_contract, s2);
}

#[test]
#[should_panic(expected = "Error(Contract, #4)")]
fn register_source_rejects_duplicates() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap);
}

#[test]
fn remove_source_drops_the_entry() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 110);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::External);
    h.aggregator.remove_source(&h.admin, &s1);
    let sources = h.aggregator.list_sources();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources.get_unchecked(0).source_contract, s2);
}

#[test]
#[should_panic(expected = "Error(Contract, #5)")]
fn remove_source_panics_on_unknown_address() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let ghost = Address::generate(&env);
    h.aggregator.remove_source(&h.admin, &ghost);
}

#[test]
fn get_price_returns_median_of_three_sources() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 110);
    let s3 = deploy_source(&env, 150);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::ClTwap);
    h.aggregator
        .register_source(&h.admin, &s3, &OracleSourceType::External);
    set_now(&env, 1_000);

    let result = h.aggregator.get_price(&h.token_a, &h.token_b);
    assert_eq!(result.price, 110);
    assert_eq!(result.confidence, 3);
}

#[test]
fn get_price_returns_two_way_median_average() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 200);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::External);
    set_now(&env, 1_000);
    let result = h.aggregator.get_price(&h.token_a, &h.token_b);
    assert_eq!(result.price, 150);
    assert_eq!(result.confidence, 2);
}

#[test]
fn stale_source_excluded_after_window() {
    let env = Env::default();
    let h = deploy(&env, 60);
    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 110);
    let s3 = deploy_source(&env, 150);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::ClTwap);
    h.aggregator
        .register_source(&h.admin, &s3, &OracleSourceType::External);

    set_now(&env, 1_000);
    h.aggregator.get_price(&h.token_a, &h.token_b);

    // Source-2 stops reporting (price goes to 0 → not counted).
    let s2_client = MockAdapterClient::new(&env, &s2);
    s2_client.set_price(&0);

    // Advance the clock past the staleness window for source-2.
    set_now(&env, 1_000 + 200);

    let result = h.aggregator.get_price(&h.token_a, &h.token_b);
    // s2 fresh-gated out (price=0 → not counted); s1 + s3 remain.
    // Median of (100, 150) = 125.
    assert_eq!(result.price, 125);
    assert_eq!(result.confidence, 2);
}

#[test]
#[should_panic(expected = "Error(Contract, #6)")]
fn single_source_below_min_panics() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap);
    set_now(&env, 1_000);
    h.aggregator.get_price(&h.token_a, &h.token_b);
}

#[test]
#[should_panic(expected = "Error(Contract, #6)")]
fn empty_registry_panics() {
    let env = Env::default();
    let h = deploy(&env, 600);
    set_now(&env, 1_000);
    h.aggregator.get_price(&h.token_a, &h.token_b);
}

#[test]
fn set_max_staleness_updates_window() {
    let env = Env::default();
    let h = deploy(&env, 600);
    h.aggregator.set_max_staleness(&h.admin, &120);
    assert_eq!(h.aggregator.get_max_staleness(), 120);
}

#[test]
#[should_panic(expected = "Error(Contract, #7)")]
fn set_max_staleness_rejects_zero() {
    let env = Env::default();
    let h = deploy(&env, 600);
    h.aggregator.set_max_staleness(&h.admin, &0);
}

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn register_source_rejects_non_admin() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let attacker = Address::generate(&env);
    let s1 = deploy_source(&env, 100);
    h.aggregator
        .register_source(&attacker, &s1, &OracleSourceType::AmmTwap);
}
