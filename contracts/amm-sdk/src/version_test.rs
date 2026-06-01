//! Tests for `emit_versioned_event!` (#302).
//!
//! Uses a tiny throwaway contract that calls the macro from inside
//! `#[contractimpl]` so the test runs through Soroban's real event
//! pipeline (not just a macro-only expansion check).

#![cfg(all(test, feature = "testutils"))]

extern crate std;

use soroban_sdk::{
    contract, contractimpl, symbol_short,
    testutils::{Address as _, Events},
    Address, Env, IntoVal,
};

use crate::EVENT_SCHEMA_VERSION;

#[contract]
struct Emitter;

#[contractimpl]
impl Emitter {
    pub fn emit_single(env: Env, a: i128) {
        crate::emit_versioned_event!(env, (symbol_short!("test_a"),), (a,));
    }

    pub fn emit_tuple(env: Env, a: Address, b: i128, c: i128) {
        crate::emit_versioned_event!(env, (symbol_short!("test_b"),), (a, b, c));
    }
}

#[test]
fn macro_prepends_schema_version_to_payload() {
    let env = Env::default();
    let contract_id = env.register_contract(None, Emitter);
    let client = EmitterClient::new(&env, &contract_id);

    client.emit_single(&42_i128);

    let events = env.events().all();
    let evt = events
        .iter()
        .find(|e| e.0 == contract_id && e.1 == (symbol_short!("test_a"),).into_val(&env))
        .expect("test_a event must be published");

    let decoded: (u32, (i128,)) = evt.2.into_val(&env);
    assert_eq!(decoded.0, EVENT_SCHEMA_VERSION);
    assert_eq!(decoded.1, (42_i128,));
}

#[test]
fn macro_preserves_topic_shape() {
    let env = Env::default();
    let contract_id = env.register_contract(None, Emitter);
    let client = EmitterClient::new(&env, &contract_id);

    let user = Address::generate(&env);
    client.emit_tuple(&user, &7_i128, &11_i128);

    let events = env.events().all();
    let evt = events
        .iter()
        .find(|e| e.0 == contract_id)
        .expect("event missing");

    // Topic stays a single-element tuple — versioning lives in the
    // payload, not the topic, so existing topic filters keep working.
    let topic: (soroban_sdk::Symbol,) = evt.1.clone().into_val(&env);
    assert_eq!(topic.0, symbol_short!("test_b"));
}

#[test]
fn schema_version_starts_at_one() {
    assert_eq!(EVENT_SCHEMA_VERSION, 1);
}
