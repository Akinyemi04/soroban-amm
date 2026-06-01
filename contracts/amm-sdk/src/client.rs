//! High-level AMM client wrapping every contract entry point.
//!
//! [`AmmPoolSdk`] is the primary entry point for on-chain usage (cross-contract
//! calls from other Soroban contracts).  Off-chain callers using a language
//! binding (e.g. stellar-sdk-rs) can use the same types for argument / return
//! value construction.

use soroban_sdk::{contractclient, Address, Bytes, BytesN, Env};

use crate::types::{LiquidityQuote, PoolInfo, SdkAmmError, SwapInQuote, SwapOutQuote};

// ── Re-export the auto-generated contract client ──────────────────────────────

/// Auto-generated client bound to the deployed AMM pool contract.
///
/// Every method signature matches the contract entry point exactly, so all
/// Rust type-checking guarantees apply before a transaction is submitted.
#[contractclient(name = "AmmPoolClient")]
pub trait AmmPoolInterface {
    fn initialize(
        env: Env,
        admin: Address,
        token_a: Address,
        token_b: Address,
        lp_token: Address,
        fee_bps: i128,
        fee_recipient: Address,
        protocol_fee_bps: i128,
    ) -> Result<(), SdkAmmError>;

    fn pause(env: Env) -> Result<(), SdkAmmError>;
    fn unpause(env: Env) -> Result<(), SdkAmmError>;
    fn is_paused(env: Env) -> bool;
    fn flash_loan_locked(env: Env) -> bool;

    fn add_liquidity(
        env: Env,
        provider: Address,
        amount_a: i128,
        amount_b: i128,
        min_shares: i128,
        deadline: u64,
    ) -> Result<i128, SdkAmmError>;

    fn remove_liquidity(
        env: Env,
        provider: Address,
        shares: i128,
        min_a: i128,
        min_b: i128,
        deadline: u64,
    ) -> Result<(i128, i128), SdkAmmError>;

    fn swap(
        env: Env,
        trader: Address,
        token_in: Address,
        amount_in: i128,
        min_out: i128,
        deadline: u64,
        referrer: Option<Address>,
    ) -> Result<i128, SdkAmmError>;

    fn swap_exact_out(
        env: Env,
        trader: Address,
        token_out: Address,
        amount_out: i128,
        max_in: i128,
        deadline: u64,
        referrer: Option<Address>,
    ) -> Result<i128, SdkAmmError>;

    fn flash_loan(
        env: Env,
        receiver: Address,
        token: Address,
        amount: i128,
        data: Bytes,
    ) -> Result<i128, SdkAmmError>;

    fn get_amount_out(env: Env, token_in: Address, amount_in: i128) -> Result<i128, SdkAmmError>;
    fn get_amount_in(env: Env, token_out: Address, amount_out: i128) -> i128;
    fn simulate_swap(env: Env, token_in: Address, amount_in: i128) -> Result<crate::types::PoolInfo, SdkAmmError>;
    fn price_ratio(env: Env) -> Result<(i128, i128), SdkAmmError>;
    fn get_info(env: Env) -> PoolInfo;
    fn get_accrued_fees(env: Env) -> (i128, i128);
    fn shares_of(env: Env, provider: Address) -> i128;
    fn get_price_cumulative(env: Env) -> (i128, i128, u64);
    fn withdraw_protocol_fees(env: Env) -> Result<(i128, i128), SdkAmmError>;

    fn update_fee(env: Env, new_fee_bps: i128) -> Result<(), SdkAmmError>;
    fn update_flash_loan_fee(env: Env, new_fee_bps: i128) -> Result<(), SdkAmmError>;
    fn set_protocol_fee(env: Env, admin: Address, recipient: Address, protocol_fee_bps: i128) -> Result<(), SdkAmmError>;
    fn get_protocol_fee(env: Env) -> (Option<Address>, i128);

    fn propose_admin(env: Env, current_admin: Address, new_admin: Address) -> Result<(), SdkAmmError>;
    fn accept_admin(env: Env, new_admin: Address) -> Result<(), SdkAmmError>;
    fn get_pending_admin(env: Env) -> Option<Address>;
    fn upgrade(env: Env, new_wasm_hash: BytesN<32>) -> Result<(), SdkAmmError>;
}

// ── High-level SDK wrapper ────────────────────────────────────────────────────

/// A high-level wrapper around [`AmmPoolClient`] that adds validated quote
/// helpers, liquidity estimation, and convenience constructors.
///
/// # Cross-contract usage
/// ```rust,ignore
/// use soroban_amm_sdk::client::AmmPoolSdk;
///
/// let sdk = AmmPoolSdk::new(&env, &pool_address);
///
/// // Validated quote — returns Err if pool is empty or token unknown
/// let quote = sdk.quote_swap_in(&token_a, 1_000_000)?;
/// if quote.price_impact_bps > 100 {
///     return Err(MyError::TooMuchSlippage);
/// }
/// sdk.execute_swap(&trader, &token_a, 1_000_000, quote.amount_out * 99 / 100, deadline, None)?;
/// ```
pub struct AmmPoolSdk {
    env: Env,
    client: AmmPoolClient,
}

impl AmmPoolSdk {
    /// Bind the SDK to a deployed pool at `pool_address`.
    pub fn new(env: &Env, pool_address: &Address) -> Self {
        Self {
            env: env.clone(),
            client: AmmPoolClient::new(env, pool_address),
        }
    }

    // ── Read-only views ───────────────────────────────────────────────────────

    /// Return full pool state.
    pub fn info(&self) -> PoolInfo {
        self.client.get_info()
    }

    /// Return `true` if the pool is administratively paused.
    pub fn paused(&self) -> bool {
        self.client.is_paused()
    }

    /// Return `true` if a flash loan is currently in progress.
    ///
    /// When `true`, all state-mutating calls will return `Reentrant`.
    pub fn flash_loan_locked(&self) -> bool {
        self.client.flash_loan_locked()
    }

    /// LP share balance of `provider`.
    pub fn shares_of(&self, provider: &Address) -> i128 {
        self.client.shares_of(provider.clone())
    }

    /// Accrued protocol fees not yet withdrawn.  Returns `(fee_a, fee_b)`.
    pub fn accrued_fees(&self) -> (i128, i128) {
        self.client.get_accrued_fees()
    }

    // ── Quote helpers ─────────────────────────────────────────────────────────

    /// Quote a swap-in: how much `token_out` you receive for `amount_in` of
    /// `token_in`.
    ///
    /// Returns `Err` if the pool is empty, paused, or `token_in` is unknown.
    /// The returned [`SwapInQuote`] includes the price impact so callers can
    /// apply their own slippage policy before submitting the real transaction.
    pub fn quote_swap_in(
        &self,
        token_in: &Address,
        amount_in: i128,
    ) -> Result<SwapInQuote, SdkAmmError> {
        if amount_in <= 0 {
            return Err(SdkAmmError::ZeroAmount);
        }
        let info = self.client.get_info();
        if info.reserve_a <= 0 || info.reserve_b <= 0 {
            return Err(SdkAmmError::EmptyPool);
        }

        let (reserve_in, reserve_out, token_out) = if *token_in == info.token_a {
            (info.reserve_a, info.reserve_b, info.token_b.clone())
        } else if *token_in == info.token_b {
            (info.reserve_b, info.reserve_a, info.token_a.clone())
        } else {
            return Err(SdkAmmError::InvalidToken);
        };

        let amount_in_with_fee = amount_in * (10_000 - info.fee_bps);
        let amount_out =
            amount_in_with_fee * reserve_out / (reserve_in * 10_000 + amount_in_with_fee);
        let fee_amount = amount_in * info.fee_bps / 10_000;
        let spot_price = reserve_out * 1_000_000 / reserve_in;
        let effective_price = if amount_in > 0 {
            amount_out * 1_000_000 / amount_in
        } else {
            0
        };
        let price_impact_bps = if spot_price > 0 {
            ((spot_price - effective_price) * 10_000 / spot_price).max(0)
        } else {
            0
        };

        Ok(SwapInQuote {
            token_in: token_in.clone(),
            token_out,
            amount_in,
            amount_out,
            fee_amount,
            spot_price,
            effective_price,
            price_impact_bps,
            valid: amount_out > 0 && amount_out < reserve_out,
        })
    }

    /// Quote a swap-out: how much `token_in` is required to receive exactly
    /// `amount_out` of `token_out`.
    ///
    /// Returns `Err` if the pool is empty, `token_out` is unknown, or
    /// `amount_out >= reserve_out`.
    pub fn quote_swap_out(
        &self,
        token_out: &Address,
        amount_out: i128,
    ) -> Result<SwapOutQuote, SdkAmmError> {
        if amount_out <= 0 {
            return Err(SdkAmmError::ZeroAmount);
        }
        let info = self.client.get_info();
        if info.reserve_a <= 0 || info.reserve_b <= 0 {
            return Err(SdkAmmError::EmptyPool);
        }

        let (reserve_in, reserve_out, token_in) = if *token_out == info.token_a {
            (info.reserve_b, info.reserve_a, info.token_b.clone())
        } else if *token_out == info.token_b {
            (info.reserve_a, info.reserve_b, info.token_a.clone())
        } else {
            return Err(SdkAmmError::InvalidToken);
        };

        if amount_out >= reserve_out {
            return Err(SdkAmmError::InsufficientLiquidity);
        }

        let required_in = (reserve_in * amount_out * 10_000)
            / ((reserve_out - amount_out) * (10_000 - info.fee_bps))
            + 1;
        let fee_amount = required_in * info.fee_bps / 10_000;

        Ok(SwapOutQuote {
            token_out: token_out.clone(),
            token_in,
            amount_out,
            required_in,
            fee_amount,
            valid: required_in > 0,
        })
    }

    /// Estimate LP shares minted for depositing `amount_a` and `amount_b`.
    ///
    /// On an empty pool (first deposit) any ratio is accepted and shares equal
    /// the geometric mean.  On a non-empty pool the lesser of the two ratios
    /// is used, matching the on-chain logic.
    pub fn quote_add_liquidity(
        &self,
        amount_a: i128,
        amount_b: i128,
    ) -> Result<LiquidityQuote, SdkAmmError> {
        if amount_a <= 0 || amount_b <= 0 {
            return Err(SdkAmmError::ZeroAmount);
        }
        let info = self.client.get_info();
        let total_shares = info.total_shares;
        let reserve_a = info.reserve_a;
        let reserve_b = info.reserve_b;

        let shares = if total_shares == 0 {
            isqrt(amount_a * amount_b)
        } else {
            let shares_a = amount_a * total_shares / reserve_a;
            let shares_b = amount_b * total_shares / reserve_b;
            shares_a.min(shares_b)
        };

        let pool_ratio = if reserve_a > 0 {
            reserve_b * 1_000_000 / reserve_a
        } else {
            0
        };

        Ok(LiquidityQuote {
            amount_a,
            amount_b,
            shares,
            pool_ratio,
        })
    }

    /// Estimate tokens returned for burning `shares` LP tokens.
    pub fn quote_remove_liquidity(&self, shares: i128) -> Result<LiquidityQuote, SdkAmmError> {
        if shares <= 0 {
            return Err(SdkAmmError::ZeroAmount);
        }
        let info = self.client.get_info();
        if info.total_shares == 0 {
            return Err(SdkAmmError::EmptyPool);
        }
        let amount_a = shares * info.reserve_a / info.total_shares;
        let amount_b = shares * info.reserve_b / info.total_shares;
        let pool_ratio = if info.reserve_a > 0 {
            info.reserve_b * 1_000_000 / info.reserve_a
        } else {
            0
        };
        Ok(LiquidityQuote {
            amount_a,
            amount_b,
            shares,
            pool_ratio,
        })
    }

    // ── State-mutating wrappers ───────────────────────────────────────────────

    /// Execute a swap with an exact input amount.
    pub fn execute_swap(
        &self,
        trader: &Address,
        token_in: &Address,
        amount_in: i128,
        min_out: i128,
        deadline: u64,
        referrer: Option<Address>,
    ) -> Result<i128, SdkAmmError> {
        self.client.swap(
            trader.clone(),
            token_in.clone(),
            amount_in,
            min_out,
            deadline,
            referrer,
        )
    }

    /// Execute a swap targeting an exact output amount.
    pub fn execute_swap_exact_out(
        &self,
        trader: &Address,
        token_out: &Address,
        amount_out: i128,
        max_in: i128,
        deadline: u64,
        referrer: Option<Address>,
    ) -> Result<i128, SdkAmmError> {
        self.client.swap_exact_out(
            trader.clone(),
            token_out.clone(),
            amount_out,
            max_in,
            deadline,
            referrer,
        )
    }

    /// Add liquidity to the pool.
    pub fn add_liquidity(
        &self,
        provider: &Address,
        amount_a: i128,
        amount_b: i128,
        min_shares: i128,
        deadline: u64,
    ) -> Result<i128, SdkAmmError> {
        self.client.add_liquidity(
            provider.clone(),
            amount_a,
            amount_b,
            min_shares,
            deadline,
        )
    }

    /// Remove liquidity from the pool.
    pub fn remove_liquidity(
        &self,
        provider: &Address,
        shares: i128,
        min_a: i128,
        min_b: i128,
        deadline: u64,
    ) -> Result<(i128, i128), SdkAmmError> {
        self.client
            .remove_liquidity(provider.clone(), shares, min_a, min_b, deadline)
    }

    /// Issue a flash loan.
    pub fn flash_loan(
        &self,
        receiver: &Address,
        token: &Address,
        amount: i128,
        data: Bytes,
    ) -> Result<i128, SdkAmmError> {
        self.client
            .flash_loan(receiver.clone(), token.clone(), amount, data)
    }
}

// ── Integer square root (mirrors on-chain sqrt) ───────────────────────────────

fn isqrt(n: i128) -> i128 {
    if n <= 0 {
        return 0;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}
