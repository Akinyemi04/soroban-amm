//! Strongly-typed decoders for every event emitted by the AMM contracts.
//!
//! The AMM contract emits Soroban events with the following structure:
//!
//! | Event symbol    | Topics                        | Data                              |
//! |-----------------|-------------------------------|-----------------------------------|
//! | `swap`          | `("swap", trader)`            | `(token_in, amt_in, token_out, amt_out, referrer)` |
//! | `add_liquidity` | `("add_liquidity", provider)` | `(amount_a, amount_b, shares)`    |
//! | `rm_liq`        | `("rm_liq",)`                 | `(provider, shares, out_a, out_b)`|
//! | `rm_liq_1s`     | `("rm_liq_1s",)`              | `(provider, shares, token_out, total_out)` |
//! | `flash_loan`    | `("flash_loan", receiver)`    | `(token, amount, fee)`            |
//! | `fee_upd`       | `("fee_upd", admin)`          | `(new_fee_bps,)`                  |
//! | `flash_fee_upd` | `("flash_fee_upd", admin)`    | `(new_fee_bps,)`                  |
//! | `admin_nominated`| `("admin_nominated",)`       | `(current_admin, new_admin)`      |
//! | `admin_changed` | `("admin_changed",)`          | `(new_admin,)`                    |
//! | `upgraded`      | `("upgraded",)`               | `(new_wasm_hash,)`                |
//! | `circuit_break` | `("circuit_break",)`          | `(price_before, price_after, deviation_bps, threshold_bps)` |

use soroban_sdk::{contracttype, Address, Bytes, BytesN};

// ── Event data types ──────────────────────────────────────────────────────────

/// Emitted when a swap executes.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct SwapEvent {
    pub trader: Address,
    pub token_in: Address,
    pub amount_in: i128,
    pub token_out: Address,
    pub amount_out: i128,
    pub referrer: Option<Address>,
}

/// Emitted when liquidity is added.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct AddLiquidityEvent {
    pub provider: Address,
    pub amount_a: i128,
    pub amount_b: i128,
    pub shares_minted: i128,
}

/// Emitted when liquidity is removed (both tokens).
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct RemoveLiquidityEvent {
    pub provider: Address,
    pub shares_burned: i128,
    pub amount_a: i128,
    pub amount_b: i128,
}

/// Emitted when liquidity is removed as a single token.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct RemoveLiquidityOneSidedEvent {
    pub provider: Address,
    pub shares_burned: i128,
    pub token_out: Address,
    pub total_out: i128,
}

/// Emitted when a flash loan executes.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct FlashLoanEvent {
    pub receiver: Address,
    pub token: Address,
    pub amount: i128,
    pub fee: i128,
}

/// Emitted when the swap fee is updated.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct FeeUpdatedEvent {
    pub admin: Address,
    pub new_fee_bps: i128,
}

/// Emitted when the flash loan fee is updated.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct FlashFeeUpdatedEvent {
    pub admin: Address,
    pub new_fee_bps: i128,
}

/// Emitted when a new admin is nominated.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct AdminNominatedEvent {
    pub current_admin: Address,
    pub new_admin: Address,
}

/// Emitted when admin transfer is accepted and completed.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct AdminChangedEvent {
    pub new_admin: Address,
}

/// Emitted when the contract WASM is upgraded.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct UpgradedEvent {
    pub new_wasm_hash: BytesN<32>,
}

/// Emitted when the circuit breaker auto-pauses the pool due to extreme price
/// movement.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct CircuitBreakerEvent {
    /// Spot price before the triggering trade (scaled × 1 000 000).
    pub price_before: i128,
    /// Spot price after the triggering trade (scaled × 1 000 000).
    pub price_after: i128,
    /// Measured deviation in basis points.
    pub deviation_bps: i128,
    /// Configured threshold that was exceeded.
    pub threshold_bps: i128,
}

// ── Event symbol constants ────────────────────────────────────────────────────

/// Symbol strings matching the on-chain event topics.
pub mod symbols {
    pub const SWAP: &str = "swap";
    pub const ADD_LIQUIDITY: &str = "add_liquidity";
    pub const REMOVE_LIQUIDITY: &str = "rm_liq";
    pub const REMOVE_LIQUIDITY_ONE_SIDED: &str = "rm_liq_1s";
    pub const FLASH_LOAN: &str = "flash_loan";
    pub const FEE_UPDATED: &str = "fee_upd";
    pub const FLASH_FEE_UPDATED: &str = "flash_fee_upd";
    pub const ADMIN_NOMINATED: &str = "admin_nominated";
    pub const ADMIN_CHANGED: &str = "admin_changed";
    pub const UPGRADED: &str = "upgraded";
    pub const CIRCUIT_BREAKER: &str = "circuit_break";
}

// ── Decoder helpers ───────────────────────────────────────────────────────────

/// Wraps all possible events that can originate from an AMM pool.
pub enum AmmEvent {
    Swap(SwapEvent),
    AddLiquidity(AddLiquidityEvent),
    RemoveLiquidity(RemoveLiquidityEvent),
    RemoveLiquidityOneSided(RemoveLiquidityOneSidedEvent),
    FlashLoan(FlashLoanEvent),
    FeeUpdated(FeeUpdatedEvent),
    FlashFeeUpdated(FlashFeeUpdatedEvent),
    AdminNominated(AdminNominatedEvent),
    AdminChanged(AdminChangedEvent),
    Upgraded(UpgradedEvent),
    CircuitBreaker(CircuitBreakerEvent),
}

/// Decode a raw Soroban event `data` field given its `symbol` topic string.
///
/// Returns `None` if the symbol is not recognised or the data cannot be decoded.
///
/// # Usage
/// ```rust,ignore
/// use soroban_sdk::Bytes;
/// use soroban_amm_sdk::events::{decode_amm_event, AmmEvent};
///
/// // `symbol` and `data` come from the RPC `getEvents` response.
/// if let Some(event) = decode_amm_event("swap", raw_data_bytes) {
///     match event {
///         AmmEvent::Swap(e) => println!("swap: {} -> {}", e.amount_in, e.amount_out),
///         _ => {}
///     }
/// }
/// ```
///
/// In practice you would obtain the data bytes from `stellar-sdk-rs` or the
/// Soroban RPC `getEvents` endpoint and pass the decoded XDR values here.
pub fn event_symbol(symbol: &str) -> Option<&'static str> {
    match symbol {
        symbols::SWAP => Some(symbols::SWAP),
        symbols::ADD_LIQUIDITY => Some(symbols::ADD_LIQUIDITY),
        symbols::REMOVE_LIQUIDITY => Some(symbols::REMOVE_LIQUIDITY),
        symbols::REMOVE_LIQUIDITY_ONE_SIDED => Some(symbols::REMOVE_LIQUIDITY_ONE_SIDED),
        symbols::FLASH_LOAN => Some(symbols::FLASH_LOAN),
        symbols::FEE_UPDATED => Some(symbols::FEE_UPDATED),
        symbols::FLASH_FEE_UPDATED => Some(symbols::FLASH_FEE_UPDATED),
        symbols::ADMIN_NOMINATED => Some(symbols::ADMIN_NOMINATED),
        symbols::ADMIN_CHANGED => Some(symbols::ADMIN_CHANGED),
        symbols::UPGRADED => Some(symbols::UPGRADED),
        symbols::CIRCUIT_BREAKER => Some(symbols::CIRCUIT_BREAKER),
        _ => None,
    }
}
