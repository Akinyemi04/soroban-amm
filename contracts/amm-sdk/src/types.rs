//! Shared types that mirror on-chain data structures.

use soroban_sdk::{contracterror, contracttype, Address};

// ── Error codes ───────────────────────────────────────────────────────────────

/// Every error that the AMM pool contract can return.
///
/// The numeric values match the on-chain `AmmError` discriminants so that
/// XDR-decoded invocation results can be round-tripped through this type.
///
/// | Code | Name                 | Cause                                               | Remedy                                            |
/// |------|----------------------|-----------------------------------------------------|---------------------------------------------------|
/// | 1    | AlreadyInitialized   | `initialize` called on a pool that is already set up | Deploy a new pool contract instead                |
/// | 2    | InvalidFeeBps        | Fee outside `[0, 10 000]` bps or protocol fee > swap fee | Use a value in the accepted range           |
/// | 3    | InsufficientShares   | LP burn amount exceeds caller's balance              | Reduce `shares` to ≤ `shares_of(provider)`       |
/// | 4    | DeadlineExceeded     | `deadline` ledger timestamp already passed           | Re-submit with a future deadline                  |
/// | 5    | SlippageExceeded     | Output or input violated the slippage guard          | Widen `min_out` / `max_in` or retry later         |
/// | 6    | Paused               | Pool is administratively paused                      | Wait for admin to call `unpause`                  |
/// | 7    | Unauthorized         | Caller does not match the stored admin               | Use the correct admin keypair                     |
/// | 8    | ZeroAmount           | Amount argument is zero or negative                  | Pass a positive value                             |
/// | 9    | InvalidToken         | `token_in`/`token_out` is not a pool token           | Use `pool.get_info()` to discover valid tokens    |
/// | 10   | EmptyPool            | One or both reserves are zero                        | Add liquidity before trading                      |
/// | 11   | InsufficientLiquidity| Output ≥ reserve or flash loan not repaid            | Reduce trade size or ensure repayment             |
/// | 12   | NoPendingAdmin       | `accept_admin` called without a prior `propose_admin`| Call `propose_admin` first                        |
/// | 13   | WrongAdmin           | `accept_admin` caller ≠ pending nominee              | Have the correct address call `accept_admin`      |
/// | 14   | Reentrant            | Reentrant call detected during flash loan callback   | Do not call pool functions from `on_flash_loan`   |
/// | 15   | CircuitBreaker       | Price moved > threshold, pool auto-paused            | Wait for cooldown or governance action            |
#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum SdkAmmError {
    AlreadyInitialized    = 1,
    InvalidFeeBps         = 2,
    InsufficientShares    = 3,
    DeadlineExceeded      = 4,
    SlippageExceeded      = 5,
    Paused                = 6,
    Unauthorized          = 7,
    ZeroAmount            = 8,
    InvalidToken          = 9,
    EmptyPool             = 10,
    InsufficientLiquidity = 11,
    NoPendingAdmin        = 12,
    WrongAdmin            = 13,
    Reentrant             = 14,
    CircuitBreaker        = 15,
}

// ── Pool state ────────────────────────────────────────────────────────────────

/// Full snapshot of an AMM pool's state returned by `get_info`.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct PoolInfo {
    pub token_a: Address,
    pub token_b: Address,
    pub reserve_a: i128,
    pub reserve_b: i128,
    pub total_shares: i128,
    pub fee_bps: i128,
    pub flash_loan_fee_bps: i128,
    pub admin: Address,
    pub fee_recipient: Address,
    pub protocol_fee_bps: i128,
}

// ── Quote types ───────────────────────────────────────────────────────────────

/// Result of a `quote_swap_in` call — how much you receive for a known input.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct SwapInQuote {
    /// Token address being sold.
    pub token_in: Address,
    /// Token address being bought.
    pub token_out: Address,
    /// Exact input amount used for the quote.
    pub amount_in: i128,
    /// Expected output amount after fees.
    pub amount_out: i128,
    /// Fee charged on the input (in input-token units).
    pub fee_amount: i128,
    /// Spot price of `token_out` in terms of `token_in` × 1 000 000.
    pub spot_price: i128,
    /// Effective execution price × 1 000 000 (amount_out / amount_in).
    pub effective_price: i128,
    /// Price impact in basis points — how far the execution price is from spot.
    pub price_impact_bps: i128,
    /// `true` if the quote is valid; `false` if the pool is empty or paused.
    pub valid: bool,
}

/// Result of a `quote_swap_out` call — how much input is required for a known output.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct SwapOutQuote {
    /// Token address being bought.
    pub token_out: Address,
    /// Token address being sold.
    pub token_in: Address,
    /// Exact desired output amount.
    pub amount_out: i128,
    /// Required input amount (before slippage guard).
    pub required_in: i128,
    /// Fee embedded in the required input.
    pub fee_amount: i128,
    /// `true` if the quote is valid.
    pub valid: bool,
}

/// Result of an `add_liquidity` or `remove_liquidity` estimation.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct LiquidityQuote {
    pub amount_a: i128,
    pub amount_b: i128,
    /// LP shares that would be minted (add) or tokens returned (remove).
    pub shares: i128,
    /// Current pool ratio: reserve_b / reserve_a × 1 000 000.
    pub pool_ratio: i128,
}

// ── Flash loan ────────────────────────────────────────────────────────────────

/// Parameters for constructing a flash loan invocation.
#[contracttype]
#[derive(Debug, Clone)]
pub struct FlashLoanParams {
    pub receiver: Address,
    pub token: Address,
    pub amount: i128,
}
