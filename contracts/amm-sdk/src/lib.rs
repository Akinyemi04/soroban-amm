//! # Soroban AMM SDK
//!
//! Type-safe Rust SDK for interacting with the Soroban AMM contracts.
//!
//! The SDK provides three layers:
//!
//! * **`types`** – shared Soroban-compatible types that mirror on-chain data
//!   structures (errors, pool state, swap results, events).
//! * **`client`** – a high-level [`AmmPoolSdk`] client that wraps every
//!   contract entry point with Rust-native ergonomics and validated quote
//!   helpers.
//! * **`events`** – strongly-typed event decoders for every event emitted by
//!   the AMM contracts.
//!
//! ## Quick start
//!
//! ```rust,ignore
//! use soroban_amm_sdk::client::AmmPoolSdk;
//! use soroban_sdk::{Address, Env};
//!
//! let env = Env::default();
//! let pool_address: Address = /* … */;
//! let sdk = AmmPoolSdk::new(&env, &pool_address);
//!
//! // Type-safe quote
//! let quote = sdk.quote_swap_in(&token_a, 1_000_000)?;
//! println!("out: {}, impact bps: {}", quote.amount_out, quote.price_impact_bps);
//! ```

#![no_std]

pub mod client;
pub mod events;
pub mod types;

#[cfg(all(test, feature = "testutils"))]
mod examples;

#[cfg(all(test, feature = "testutils"))]
mod version_test;

// ── Event schema versioning (#302) ──────────────────────────────────────────
//
// Every event the AMM / CL / governance / factory contracts emit
// goes through `emit_versioned_event!`, which stamps the payload
// with `EVENT_SCHEMA_VERSION` at index 0. Indexers / off-chain
// consumers read `(version, ...rest)`:
//
//   - On startup, refuse versions newer than the one the consumer
//     was compiled against (or fall back to "drop event" depending
//     on policy).
//   - Bump `EVENT_SCHEMA_VERSION` when ANY event payload changes
//     shape (added field, renamed field, type change).
//
// The version is intentionally a single global, not per-event. A
// per-event version would let one event's shape drift independently
// of the others, which makes consumer logic harder to maintain. One
// version-bump per release matches how the contracts are deployed
// (a workspace-wide soroban release).

pub const EVENT_SCHEMA_VERSION: u32 = 1;

/// Stamp a contract event with the current `EVENT_SCHEMA_VERSION`
/// and publish it. Drop-in replacement for `env.events().publish(...)`
/// at every emit site in the AMM / CL / governance / factory crates.
///
/// Expansion:
///
/// ```ignore
/// emit_versioned_event!(env, (topic,), (a, b, c));
/// // → env.events().publish((topic,), (EVENT_SCHEMA_VERSION, (a, b, c)));
/// ```
///
/// Consumers read `(version: u32, payload)` and pattern-match on
/// `version` to pick the right decoder.
#[macro_export]
macro_rules! emit_versioned_event {
    ($env:expr, $topic:expr, $payload:expr) => {{
        $env.events()
            .publish($topic, ($crate::EVENT_SCHEMA_VERSION, $payload));
    }};
}
