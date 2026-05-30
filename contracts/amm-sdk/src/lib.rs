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
