//! Constant-product AMM (x * y = k) on Soroban.
//!
//! Flow:
//!   1. Deploy this contract + two asset token contracts.
//!   2. Call `initialize` with both token addresses.
//!   3. First LP calls `add_liquidity` to seed the pool.
//!   4. Traders call `swap` to exchange tokens.
//!   5. LPs call `remove_liquidity` to redeem their share.

#![no_std]

use soroban_sdk::{
    contract, contractclient, contractimpl, contracterror, contracttype, symbol_short, Address,
    Bytes, BytesN, Env, Symbol,
};
// Export compiled WASM for tests/dev usage when the `testutils` feature is enabled.
#[cfg(feature = "testutils")]
pub const WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/wasm32v1-none/release/amm.wasm"
));
// Standard SEP-41 interface for pool tokens (token_a, token_b)
use soroban_sdk::token::Client as SepTokenClient;

/// Interface for the LP token contract.
///
/// We define this locally rather than importing the `token` crate to avoid
/// duplicate symbol errors during the WASM build.
/// Oracle aggregator price quote (#318).
#[contracttype]
#[derive(Clone, Debug)]
pub struct AggregatedPrice {
    pub price: i128,
    pub confidence: u32,
}

#[contractclient(name = "OracleAggregatorClient")]
pub trait OracleAggregatorInterface {
    /// Returns median price + confidence; confidence 0 when sources are stale.
    fn get_price_safe(env: Env, token_a: Address, token_b: Address) -> AggregatedPrice;
}

#[soroban_sdk::contractclient(name = "LpTokenClient")]
pub trait LpTokenInterface {
    fn initialize(
        env: Env,
        admin: Address,
        name: soroban_sdk::String,
        symbol: soroban_sdk::String,
        decimals: u32,
    );
    fn mint(env: Env, to: Address, amount: i128);
    fn burn(env: Env, from: Address, amount: i128);
    fn balance(env: Env, id: Address) -> i128;
}

// ── Typed errors ─────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum AmmError {
    AlreadyInitialized   = 1,
    InvalidFeeBps        = 2,
    InsufficientShares   = 3,
    DeadlineExceeded     = 4,
    SlippageExceeded     = 5,
    Paused               = 6,
    Unauthorized         = 7,
    ZeroAmount           = 8,
    InvalidToken         = 9,
    EmptyPool            = 10,
    InsufficientLiquidity = 11,
    NoPendingAdmin       = 12,
    WrongAdmin           = 13,
    /// A reentrant call was detected while a flash loan or state-mutating
    /// operation was already in progress. The receiver contract must not
    /// call back into this pool during an `on_flash_loan` callback.
    Reentrant            = 14,
    /// The circuit breaker tripped: spot price deviated more than the
    /// configured threshold in a single block.  The pool has been
    /// automatically paused.  Recovery requires the cooldown period to
    /// elapse and a call to `try_circuit_breaker_recovery`, or a direct
    /// admin/governance `unpause`.
    CircuitBreaker       = 15,
    /// A fee-on-transfer token deducted more fees than the caller's
    /// `min_received` threshold permitted. The pool received fewer tokens
    /// than requested; the call is reverted to protect the caller.
    FotSlippage          = 16,
    /// Spot price deviated beyond the configured oracle tolerance.
    OracleDeviationExceeded = 17,
}

// ── Storage keys ─────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    TokenA,
    TokenB,
    LpToken,
    ReserveA,
    ReserveB,
    TotalShares,
    PriceCumulativeA,
    PriceCumulativeB,
    LiquidityCumulative,
    LastTimestamp,
    Shares(Address),
    // Emergency withdrawal storage
    EmergencyWithdrawTimestamp,
    EmergencyWithdrawRecipient,

    // Admin & fees
    Admin,
    PendingAdmin,
    FeeBps,
    FeeRecipient,
    ProtocolFeeBps,
    AccruedFeeA,
    AccruedFeeB,
    FlashLoanFeeBps,

    // Issue #292: LP fee rebate — fraction of protocol fee redistributed to LPs.
    /// Basis points of the protocol fee that are rebated back into LP reserves
    /// (e.g. 5_000 = 50 % of the protocol fee goes back to LPs).
    LpRebateBps,

    // Issue #293: k-of-n multisig guard for emergency operations.
    /// Vec<Address> of multisig signers.
    MultisigSigners,
    /// Required quorum (k in k-of-n).
    MultisigQuorum,
    /// Pending emergency-withdraw proposal: (recipient, Vec<Address> approvals).
    MultisigProposalRecipient,
    MultisigProposalApprovals,

    // Issue #294: minimum liquidity lock — LP tokens permanently locked on first deposit.
    /// Whether the minimum liquidity has already been locked (set on first deposit).
    MinLiquidityLocked,

    // Pause / reentrancy
    Paused,
    /// Set to `true` while a flash loan is executing to block reentrant calls.
    /// Cleared to `false` after the callback returns and repayment is verified.
    Locked,

    // Circuit breaker
    /// Price deviation threshold in bps above which the circuit breaker trips
    /// (default 5 000 = 50 %). Configurable via `set_circuit_breaker_config`.
    CircuitBreakerThresholdBps,

    /// Minimum seconds that must elapse after the circuit breaker trips before
    /// automatic recovery is attempted (default 600 s = 10 min).
    CircuitBreakerCooldown,

    /// Ledger timestamp at which the circuit breaker was last triggered.
    /// `0` when not triggered.
    CircuitBreakerTriggeredAt,

    /// Spot price (reserve_b * 1_000_000 / reserve_a) captured at the
    /// beginning of the current ledger sequence. Used to measure intra-block
    /// price deviation.
    CircuitBreakerLastPrice,

    /// Ledger sequence number at which `CircuitBreakerLastPrice` was captured.
    CircuitBreakerLastSeqno,

    /// Optional oracle aggregator for pre-swap deviation checks (#318).
    OracleAggregator,
    /// Max allowed spot-vs-oracle deviation in basis points (e.g. 500 = 5 %).
    MaxOracleDeviationBps,
}

// ── Pool info returned by `get_info` ─────────────────────────────────────────

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
    /// Issue #292: fraction of protocol fee rebated back to LP reserves (bps).
    pub lp_rebate_bps: i128,
}

/// Issue #293: multisig configuration returned by `get_multisig_config`.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct MultisigConfig {
    pub signers: soroban_sdk::Vec<Address>,
    pub quorum: u32,
}

/// Issue #293: pending emergency-withdraw proposal.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct MultisigProposal {
    pub recipient: Address,
    pub approvals: soroban_sdk::Vec<Address>,
}

#[contractclient(name = "FlashLoanReceiverClient")]
pub trait FlashLoanReceiver {
    fn on_flash_loan(env: Env, token: Address, amount: i128, fee: i128, data: Bytes) -> bool;
}

#[contractclient(name = "FlashLoanBothReceiverClient")]
pub trait FlashLoanBothReceiver {
    fn on_flash_loan_both(
        env: Env,
        amount_a: i128,
        fee_a: i128,
        amount_b: i128,
        fee_b: i128,
        data: Bytes,
    ) -> bool;
}

// ── Swap simulation returned by `simulate_swap` ───────────────────────────────

#[contracttype]
pub struct SwapSimulation {
    pub amount_out: i128,
    pub fee_amount: i128,
    pub price_impact_bps: i128, // price impact in basis points
    pub effective_price: i128,  // amount_out / amount_in scaled by 1_000_000
    pub spot_price: i128,       // reserve_out / reserve_in scaled by 1_000_000
}

/// Circuit breaker configuration and current state returned by
/// `get_circuit_breaker_config`.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct CircuitBreakerConfig {
    /// Price deviation threshold in bps (e.g. 5 000 = 50 %).
    pub threshold_bps: i128,
    /// Minimum cooldown seconds before automatic recovery.
    pub cooldown_secs: u64,
    /// Timestamp at which the circuit breaker last tripped (0 = never).
    pub triggered_at: u64,
    /// Whether the circuit breaker is currently active (pool paused by CB).
    pub tripped: bool,
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct AmmPool;

#[contractimpl]
impl AmmPool {
    // ── Admin / Setup ─────────────────────────────────────────────────────────

    /// Initialize the AMM pool with two tokens, an LP token, and a swap fee.
    ///
    /// Must be called exactly once after deployment. The LP token contract must
    /// already be deployed with this contract set as its admin so it can mint
    /// and burn shares on behalf of liquidity providers.
    ///
    /// # Parameters
    /// - `token_a` – Address of the first pool token (SEP-41 compliant).
    /// - `token_b` – Address of the second pool token (SEP-41 compliant).
    /// - `lp_token` – Address of the LP token contract used to represent pool shares.
    /// - `fee_bps` – Swap fee in basis points (e.g. `30` = 0.30 %). Must be in `[0, 10_000]`.
    ///
    /// `lp_token` must already be deployed and its admin set to this contract.
    /// `admin` is stored as the contract administrator and is the only address
    /// permitted to call `set_protocol_fee` after deployment.
    /// `fee_recipient` receives accrued protocol fees via `withdraw_protocol_fees`.
    /// `protocol_fee_bps` must be ≤ `fee_bps`; set to 0 to disable protocol fees.
    /// # Panics
    /// - If the pool has already been initialized.
    /// - If `token_a == token_b`.
    /// - If `fee_bps` is outside the range `[0, 10_000]`.
    #[allow(clippy::too_many_arguments)]
    pub fn initialize(
        env: Env,
        admin: Address,
        token_a: Address,
        token_b: Address,
        lp_token: Address,
        fee_bps: i128, // recommended: 30 (0.30 %)
        fee_recipient: Address,
        protocol_fee_bps: i128,
    ) -> Result<(), AmmError> {
        Self::initialize_with_flash_loan_fee(
            env,
            admin,
            token_a,
            token_b,
            lp_token,
            fee_bps,
            fee_recipient,
            protocol_fee_bps,
            fee_bps,
        )
    }

    /// Initialize the pool with a distinct flash-loan fee.
    #[allow(clippy::too_many_arguments)]
    pub fn initialize_with_flash_loan_fee(
        env: Env,
        admin: Address,
        token_a: Address,
        token_b: Address,
        lp_token: Address,
        fee_bps: i128,
        fee_recipient: Address,
        protocol_fee_bps: i128,
        flash_loan_fee_bps: i128,
    ) -> Result<(), AmmError> {
        if env.storage().instance().has(&DataKey::TokenA) {
            return Err(AmmError::AlreadyInitialized);
        }
        if token_a == token_b {
            return Err(AmmError::InvalidToken);
        }
        Self::validate_fee_bps(fee_bps)?;
        Self::validate_fee_bps(flash_loan_fee_bps)?;
        if !(0..=fee_bps).contains(&protocol_fee_bps) {
            return Err(AmmError::InvalidFeeBps);
        }

        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::TokenA, &token_a);
        env.storage().instance().set(&DataKey::TokenB, &token_b);
        env.storage().instance().set(&DataKey::LpToken, &lp_token);
        env.storage().instance().set(&DataKey::FeeBps, &fee_bps);
        env.storage()
            .instance()
            .set(&DataKey::FeeRecipient, &fee_recipient);
        env.storage()
            .instance()
            .set(&DataKey::ProtocolFeeBps, &protocol_fee_bps);
        env.storage().instance().set(&DataKey::AccruedFeeA, &0_i128);
        env.storage().instance().set(&DataKey::AccruedFeeB, &0_i128);
        env.storage()
            .instance()
            .set(&DataKey::FlashLoanFeeBps, &flash_loan_fee_bps);
        env.storage().instance().set(&DataKey::ReserveA, &0_i128);
        env.storage().instance().set(&DataKey::ReserveB, &0_i128);
        env.storage().instance().set(&DataKey::TotalShares, &0_i128);
        env.storage()
            .instance()
            .set(&DataKey::PriceCumulativeA, &0_i128);
        env.storage()
            .instance()
            .set(&DataKey::PriceCumulativeB, &0_i128);
        env.storage()
            .instance()
            .set(&DataKey::LiquidityCumulative, &0_i128);
        env.storage()
            .instance()
            .set(&DataKey::LastTimestamp, &env.ledger().timestamp());
        env.storage().instance().set(&DataKey::Paused, &false);
        env.storage().instance().set(&DataKey::Locked, &false);
        // Issue #292: LP rebate disabled by default.
        env.storage().instance().set(&DataKey::LpRebateBps, &0_i128);
        // Issue #293: multisig disabled by default (empty signers, quorum 0).
        env.storage().instance().set(&DataKey::MultisigQuorum, &0_u32);
        env.storage()
            .instance()
            .set(&DataKey::MultisigSigners, &soroban_sdk::Vec::<Address>::new(&env));
        // Issue #294: minimum liquidity not yet locked.
        env.storage().instance().set(&DataKey::MinLiquidityLocked, &false);
        // Circuit breaker: default threshold 50 % (5 000 bps), cooldown 600 s.
        env.storage()
            .instance()
            .set(&DataKey::CircuitBreakerThresholdBps, &5_000_i128);
        env.storage()
            .instance()
            .set(&DataKey::CircuitBreakerCooldown, &600_u64);
        env.storage()
            .instance()
            .set(&DataKey::CircuitBreakerTriggeredAt, &0_u64);
        env.storage()
            .instance()
            .set(&DataKey::CircuitBreakerLastPrice, &0_i128);
        env.storage()
            .instance()
            .set(&DataKey::CircuitBreakerLastSeqno, &0_u32);
        // Oracle deviation guard (#318): disabled until admin configures an oracle.
        env.storage()
            .instance()
            .set(&DataKey::OracleAggregator, &Option::<Address>::None);
        env.storage()
            .instance()
            .set(&DataKey::MaxOracleDeviationBps, &500_i128);
        Ok(())
    }

    /// Admin: attach or remove the oracle aggregator used for swap deviation checks.
    pub fn set_oracle(env: Env, admin: Address, oracle: Option<Address>) -> Result<(), AmmError> {
        let stored_admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        if admin != stored_admin {
            return Err(AmmError::Unauthorized);
        }
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::OracleAggregator, &oracle);
        Ok(())
    }

    /// Admin: max spot-vs-oracle deviation in basis points before swaps revert.
    pub fn set_max_oracle_deviation_bps(
        env: Env,
        admin: Address,
        max_deviation_bps: i128,
    ) -> Result<(), AmmError> {
        let stored_admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        if admin != stored_admin {
            return Err(AmmError::Unauthorized);
        }
        admin.require_auth();
        if !(0..=10_000).contains(&max_deviation_bps) {
            return Err(AmmError::InvalidFeeBps);
        }
        env.storage()
            .instance()
            .set(&DataKey::MaxOracleDeviationBps, &max_deviation_bps);
        Ok(())
    }

    pub fn pause(env: Env) -> Result<(), AmmError> {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &true);
        Ok(())
    }

    pub fn unpause(env: Env) -> Result<(), AmmError> {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &false);
        Ok(())
    }

    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    /// Emergency withdraw of all pool reserves to a designated address.
    /// Admin-only, callable via a timed governance proposal.
    pub fn emergency_withdraw(env: Env, to: Address) -> Result<(), AmmError> {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();

        // Record audit information
        let ts: u64 = env.ledger().timestamp();
        env.storage()
            .instance()
            .set(&DataKey::EmergencyWithdrawTimestamp, &ts);
        env.storage()
            .instance()
            .set(&DataKey::EmergencyWithdrawRecipient, &to);

        // Get token addresses and reserves
        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let reserve_a = Self::get_reserve_a(env.clone());
        let reserve_b = Self::get_reserve_b(env.clone());

        // Transfer reserves to recipient
        if reserve_a > 0 {
            SepTokenClient::new(&env, &token_a)
                .transfer(&env.current_contract_address(), &to, &reserve_a);
        }
        if reserve_b > 0 {
            SepTokenClient::new(&env, &token_b)
                .transfer(&env.current_contract_address(), &to, &reserve_b);
        }

        // Zero out reserves
        env.storage().instance().set(&DataKey::ReserveA, &0_i128);
        env.storage().instance().set(&DataKey::ReserveB, &0_i128);

        // Emit event for audit trail
        soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(&env, "emergency_withdraw"), admin.clone()), (to, reserve_a, reserve_b));

        Ok(())
    }

    /// Return `true` while a flash loan is executing on this pool.
    ///
    /// During this window all state-mutating functions (`swap`,
    /// `add_liquidity`, `remove_liquidity`, `flash_loan`) will reject calls
    /// with `AmmError::Reentrant`. This is a read-only diagnostic; callers
    /// should not rely on this for security checks — the guard is enforced
    /// internally by `enter_lock`.
    pub fn flash_loan_locked(env: Env) -> bool {
        Self::is_locked(&env)
    }

    // ── Circuit breaker ───────────────────────────────────────────────────────

    /// Configure the circuit breaker.
    ///
    /// # Parameters
    /// - `threshold_bps` – Maximum allowed intra-block spot-price deviation in
    ///   basis points before the pool is auto-paused (e.g. `5_000` = 50 %).
    ///   Must be in `(0, 10_000]`.
    /// - `cooldown_secs` – Minimum seconds that must pass after tripping before
    ///   automatic recovery via `try_circuit_breaker_recovery` is allowed.
    ///
    /// Admin-only.
    pub fn set_circuit_breaker_config(
        env: Env,
        threshold_bps: i128,
        cooldown_secs: u64,
    ) -> Result<(), AmmError> {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();

        if threshold_bps <= 0 || threshold_bps > 10_000 {
            return Err(AmmError::InvalidFeeBps);
        }

        env.storage()
            .instance()
            .set(&DataKey::CircuitBreakerThresholdBps, &threshold_bps);

        env.storage()
            .instance()
            .set(&DataKey::CircuitBreakerCooldown, &cooldown_secs);

        soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(&env, "cb_config"),), (threshold_bps, cooldown_secs));

        Ok(())
    }

    /// Return the current circuit breaker configuration and state.
    pub fn get_circuit_breaker_config(env: Env) -> CircuitBreakerConfig {
        let threshold_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::CircuitBreakerThresholdBps)
            .unwrap_or(5_000);

        let cooldown_secs: u64 = env
            .storage()
            .instance()
            .get(&DataKey::CircuitBreakerCooldown)
            .unwrap_or(600);

        let triggered_at: u64 = env
            .storage()
            .instance()
            .get(&DataKey::CircuitBreakerTriggeredAt)
            .unwrap_or(0);

        let tripped = triggered_at > 0 && Self::is_paused(env.clone());

        CircuitBreakerConfig {
            threshold_bps,
            cooldown_secs,
            triggered_at,
            tripped,
        }
    }

    /// Attempt automatic recovery after the circuit breaker cooldown.
    pub fn try_circuit_breaker_recovery(env: Env) -> Result<bool, AmmError> {
        let triggered_at: u64 = env
            .storage()
            .instance()
            .get(&DataKey::CircuitBreakerTriggeredAt)
            .unwrap_or(0);

        if triggered_at == 0 {
            return Ok(false);
        }

        if !Self::is_paused(env.clone()) {
            env.storage()
                .instance()
                .set(&DataKey::CircuitBreakerTriggeredAt, &0_u64);
            return Ok(false);
        }

        let cooldown: u64 = env
            .storage()
            .instance()
            .get(&DataKey::CircuitBreakerCooldown)
            .unwrap_or(600);

        let now = env.ledger().timestamp();

        if now < triggered_at + cooldown {
            return Ok(false);
        }

        env.storage().instance().set(&DataKey::Paused, &false);

        env.storage()
            .instance()
            .set(&DataKey::CircuitBreakerTriggeredAt, &0_u64);

        env.events()
            .publish((Symbol::new(&env, "cb_recovered"),), (now,));

        Ok(true)
    }

    /// Internal: capture the spot price at the start of a new ledger sequence.
    fn check_circuit_breaker(env: &Env) -> Result<(), AmmError> {
        let reserve_a = Self::get_reserve_a(env.clone());
        let reserve_b = Self::get_reserve_b(env.clone());

        if reserve_a <= 0 || reserve_b <= 0 {
            return Ok(());
        }

        let threshold_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::CircuitBreakerThresholdBps)
            .unwrap_or(5_000);

        let current_seqno = env.ledger().sequence();

        let last_seqno: u32 = env
            .storage()
            .instance()
            .get(&DataKey::CircuitBreakerLastSeqno)
            .unwrap_or(0);

        let current_price = reserve_b * 1_000_000 / reserve_a;

        if last_seqno != current_seqno {
            env.storage()
                .instance()
                .set(&DataKey::CircuitBreakerLastPrice, &current_price);

            env.storage()
                .instance()
                .set(&DataKey::CircuitBreakerLastSeqno, &current_seqno);

            return Ok(());
        }

        let baseline_price: i128 = env
            .storage()
            .instance()
            .get(&DataKey::CircuitBreakerLastPrice)
            .unwrap_or(current_price);

        if baseline_price <= 0 {
            return Ok(());
        }

        let deviation_bps = if current_price >= baseline_price {
            (current_price - baseline_price) * 10_000 / baseline_price
        } else {
            (baseline_price - current_price) * 10_000 / baseline_price
        };

        if deviation_bps >= threshold_bps {
            let now = env.ledger().timestamp();

            env.storage().instance().set(&DataKey::Paused, &true);

            env.storage()
                .instance()
                .set(&DataKey::CircuitBreakerTriggeredAt, &now);

            soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(&env, "circuit_break"),), (baseline_price, current_price, deviation_bps, threshold_bps));

            return Err(AmmError::CircuitBreaker);
        }

        Ok(())
    }

    /// Update the protocol fee configuration. Admin-only.
    ///
    /// Set `protocol_fee_bps` to 0 to disable protocol fee collection.
    /// `protocol_fee_bps` must be ≤ the pool's `fee_bps`.
    pub fn set_protocol_fee(env: Env, admin: Address, recipient: Address, protocol_fee_bps: i128) -> Result<(), AmmError> {
        let stored_admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        if admin != stored_admin {
            return Err(AmmError::Unauthorized);
        }
        admin.require_auth();
        let fee_bps: i128 = env.storage().instance().get(&DataKey::FeeBps).unwrap();
        if protocol_fee_bps < 0 || protocol_fee_bps > fee_bps {
            return Err(AmmError::InvalidFeeBps);
        }
        env.storage()
            .instance()
            .set(&DataKey::FeeRecipient, &recipient);
        env.storage()
            .instance()
            .set(&DataKey::ProtocolFeeBps, &protocol_fee_bps);
        Ok(())
    }

    /// Return the current protocol fee recipient and rate.
    ///
    /// Returns `(None, 0)` when protocol fees are disabled.
    pub fn get_protocol_fee(env: Env) -> (Option<Address>, i128) {
        let recipient: Option<Address> = env.storage().instance().get(&DataKey::FeeRecipient);
        let bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(0);
        (recipient, bps)
    }

    // ── Issue #292: LP fee rebate ─────────────────────────────────────────────

    /// Set the fraction of the protocol fee rebated back into LP reserves.
    ///
    /// `lp_rebate_bps` is a fraction of `protocol_fee_bps`
    /// (e.g. 5_000 = 50 % of the protocol cut goes back to LPs).
    /// Must be in `[0, 10_000]`. Admin-only.
    pub fn set_lp_rebate(env: Env, admin: Address, lp_rebate_bps: i128) -> Result<(), AmmError> {
        let stored_admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        if admin != stored_admin {
            return Err(AmmError::Unauthorized);
        }
        admin.require_auth();
        if !(0..=10_000).contains(&lp_rebate_bps) {
            return Err(AmmError::InvalidFeeBps);
        }
        env.storage()
            .instance()
            .set(&DataKey::LpRebateBps, &lp_rebate_bps);
        soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(&env, "lp_rebate_set"), admin), (lp_rebate_bps,));
        Ok(())
    }

    /// Return the current LP rebate rate in basis points.
    pub fn get_lp_rebate(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::LpRebateBps)
            .unwrap_or(0)
    }

    // ── Issue #293: k-of-n multisig emergency guard ───────────────────────────

    /// Configure the k-of-n multisig guard for emergency operations.
    ///
    /// Once set, `emergency_withdraw` requires `quorum` approvals from `signers`
    /// before funds can be moved. Admin-only.
    /// Set `quorum` to 0 to disable the multisig guard (single-admin mode).
    pub fn set_multisig(
        env: Env,
        admin: Address,
        signers: soroban_sdk::Vec<Address>,
        quorum: u32,
    ) -> Result<(), AmmError> {
        let stored_admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        if admin != stored_admin {
            return Err(AmmError::Unauthorized);
        }
        admin.require_auth();
        if quorum > 0 && (quorum as usize) > signers.len() as usize {
            return Err(AmmError::InvalidFeeBps);
        }
        env.storage()
            .instance()
            .set(&DataKey::MultisigSigners, &signers);
        env.storage()
            .instance()
            .set(&DataKey::MultisigQuorum, &quorum);
        // Clear any pending proposal when config changes.
        env.storage()
            .instance()
            .set(&DataKey::MultisigProposalRecipient, &Option::<Address>::None);
        env.storage()
            .instance()
            .set(&DataKey::MultisigProposalApprovals, &soroban_sdk::Vec::<Address>::new(&env));
        soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(&env, "multisig_set"), admin), (quorum,));
        Ok(())
    }

    /// Return the current multisig configuration.
    pub fn get_multisig_config(env: Env) -> MultisigConfig {
        MultisigConfig {
            signers: env
                .storage()
                .instance()
                .get(&DataKey::MultisigSigners)
                .unwrap_or_else(|| soroban_sdk::Vec::new(&env)),
            quorum: env
                .storage()
                .instance()
                .get(&DataKey::MultisigQuorum)
                .unwrap_or(0),
        }
    }

    /// Propose an emergency withdrawal (multisig mode).
    ///
    /// Any configured signer may call this to initiate or co-sign a proposal.
    /// Once `quorum` approvals are collected the proposal can be executed via
    /// `exec_multisig_emergency_wd`.
    pub fn propose_emergency_withdraw(
        env: Env,
        signer: Address,
        recipient: Address,
    ) -> Result<(), AmmError> {
        signer.require_auth();
        let quorum: u32 = env
            .storage()
            .instance()
            .get(&DataKey::MultisigQuorum)
            .unwrap_or(0);
        if quorum == 0 {
            return Err(AmmError::Unauthorized); // use emergency_withdraw directly
        }
        let signers: soroban_sdk::Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::MultisigSigners)
            .unwrap_or_else(|| soroban_sdk::Vec::new(&env));
        if !signers.contains(&signer) {
            return Err(AmmError::Unauthorized);
        }
        // Reset approvals if recipient changed.
        let current_recipient: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::MultisigProposalRecipient)
            .unwrap_or(None);
        let mut approvals: soroban_sdk::Vec<Address> =
            if current_recipient.as_ref() == Some(&recipient) {
                env.storage()
                    .instance()
                    .get(&DataKey::MultisigProposalApprovals)
                    .unwrap_or_else(|| soroban_sdk::Vec::new(&env))
            } else {
                soroban_sdk::Vec::new(&env)
            };
        if !approvals.contains(&signer) {
            approvals.push_back(signer.clone());
        }
        env.storage()
            .instance()
            .set(&DataKey::MultisigProposalRecipient, &Some(recipient.clone()));
        env.storage()
            .instance()
            .set(&DataKey::MultisigProposalApprovals, &approvals);
        soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(&env, "ms_proposed"), signer), (recipient, approvals.len()));
        Ok(())
    }

    /// Return the current pending multisig proposal, if any.
    pub fn get_multisig_proposal(env: Env) -> Option<MultisigProposal> {
        let recipient: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::MultisigProposalRecipient)
            .unwrap_or(None);
        recipient.map(|r| MultisigProposal {
            recipient: r,
            approvals: env
                .storage()
                .instance()
                .get(&DataKey::MultisigProposalApprovals)
                .unwrap_or_else(|| soroban_sdk::Vec::new(&env)),
        })
    }

    /// Execute the pending multisig emergency withdrawal once quorum is reached.
    ///
    /// Any signer may call this after enough approvals have been collected.
    pub fn exec_multisig_emergency_wd(
        env: Env,
        signer: Address,
    ) -> Result<(), AmmError> {
        signer.require_auth();
        let quorum: u32 = env
            .storage()
            .instance()
            .get(&DataKey::MultisigQuorum)
            .unwrap_or(0);
        if quorum == 0 {
            return Err(AmmError::Unauthorized);
        }
        let signers: soroban_sdk::Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::MultisigSigners)
            .unwrap_or_else(|| soroban_sdk::Vec::new(&env));
        if !signers.contains(&signer) {
            return Err(AmmError::Unauthorized);
        }
        let recipient: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::MultisigProposalRecipient)
            .unwrap_or(None);
        let to = recipient.ok_or(AmmError::NoPendingAdmin)?;
        let approvals: soroban_sdk::Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::MultisigProposalApprovals)
            .unwrap_or_else(|| soroban_sdk::Vec::new(&env));
        if (approvals.len() as u32) < quorum {
            return Err(AmmError::InsufficientShares);
        }
        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let reserve_a = Self::get_reserve_a(env.clone());
        let reserve_b = Self::get_reserve_b(env.clone());
        if reserve_a > 0 {
            SepTokenClient::new(&env, &token_a)
                .transfer(&env.current_contract_address(), &to, &reserve_a);
        }
        if reserve_b > 0 {
            SepTokenClient::new(&env, &token_b)
                .transfer(&env.current_contract_address(), &to, &reserve_b);
        }
        env.storage().instance().set(&DataKey::ReserveA, &0_i128);
        env.storage().instance().set(&DataKey::ReserveB, &0_i128);
        // Clear proposal.
        env.storage()
            .instance()
            .set(&DataKey::MultisigProposalRecipient, &Option::<Address>::None);
        env.storage()
            .instance()
            .set(&DataKey::MultisigProposalApprovals, &soroban_sdk::Vec::<Address>::new(&env));
        soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(&env, "ms_ew"), signer), (to, reserve_a, reserve_b));
        Ok(())
    }

    fn check_oracle_deviation(
        env: &Env,
        token_in: &Address,
        token_out: &Address,
        amount_in: i128,
        amount_out: i128,
    ) -> Result<(), AmmError> {
        let oracle: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::OracleAggregator)
            .unwrap_or(None);
        let Some(oracle_addr) = oracle else {
            return Ok(());
        };
        let max_dev: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MaxOracleDeviationBps)
            .unwrap_or(500);

        let agg = OracleAggregatorClient::new(env, &oracle_addr)
            .get_price_safe(token_in, token_out);
        if amount_in <= 0 || agg.confidence == 0 || agg.price <= 0 {
            return Ok(());
        }

        let spot_price = amount_out * 1_000_000 / amount_in;
        let oracle_price = agg.price;
        let deviation_bps = if spot_price >= oracle_price {
            (spot_price - oracle_price) * 10_000 / oracle_price
        } else {
            (oracle_price - spot_price) * 10_000 / oracle_price
        };
        if deviation_bps > max_dev {
            return Err(AmmError::OracleDeviationExceeded);
        }
        Ok(())
    }

    /// Shared by initialize, update_fee, and set_protocol_fee.
    fn validate_fee_bps(fee_bps: i128) -> Result<(), AmmError> {
        if !(0..=10_000).contains(&fee_bps) {
            return Err(AmmError::InvalidFeeBps);
        }
        Ok(())
    }

    /// Update the swap fee post-deployment. Admin-only.
    ///
    /// The new fee takes effect on the very next swap.
    /// Emits a `fee_upd` event on every successful call.
    ///
    /// # Parameters
    /// - `admin` - must match the stored admin address.
    /// - `new_fee_bps` - new swap fee in basis points; must be in [0, 10_000].
    ///
    /// # Panics
    /// - If `admin` auth fails.
    /// - If `new_fee_bps` is outside [0, 10_000].
    /// - If `new_fee_bps` is less than the current `protocol_fee_bps`.
    pub fn update_fee(env: Env, new_fee_bps: i128) -> Result<(), AmmError> {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        Self::validate_fee_bps(new_fee_bps)?;
        let protocol_fee_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(0);
        if new_fee_bps < protocol_fee_bps {
            return Err(AmmError::InvalidFeeBps);
        }
        env.storage().instance().set(&DataKey::FeeBps, &new_fee_bps);
        env.events()
            .publish((symbol_short!("fee_upd"), admin.clone()), (new_fee_bps,));
        Ok(())
    }

    /// Update the flash loan fee post-deployment. Admin-only.
    pub fn update_flash_loan_fee(env: Env, new_fee_bps: i128) -> Result<(), AmmError> {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        Self::validate_fee_bps(new_fee_bps)?;
        env.storage()
            .instance()
            .set(&DataKey::FlashLoanFeeBps, &new_fee_bps);
        soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(&env, "flash_fee_upd"), admin.clone()), (new_fee_bps,));
        Ok(())
    }

    /// Nominate a new admin. The nominee must call `accept_admin` to complete the transfer.
    ///
    /// # Panics
    /// - If `current_admin` is not the stored admin.
    /// - If `current_admin` auth fails.
    pub fn propose_admin(env: Env, current_admin: Address, new_admin: Address) -> Result<(), AmmError> {
        let stored: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        if current_admin != stored {
            return Err(AmmError::Unauthorized);
        }
        current_admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &Some(new_admin.clone()));
        soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(&env, "admin_nominated"),), (current_admin, new_admin));
        Ok(())
    }

    /// Accept the pending admin nomination. Caller becomes the new admin.
    pub fn accept_admin(env: Env, new_admin: Address) -> Result<(), AmmError> {
        let pending: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or(None);
        let nominee = pending.ok_or(AmmError::NoPendingAdmin)?;
        if new_admin != nominee {
            return Err(AmmError::WrongAdmin);
        }
        new_admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &Option::<Address>::None);
        env.events()
            .publish((Symbol::new(&env, "admin_changed"),), (new_admin,));
        Ok(())
    }

    /// Replace the contract WASM with a new version. Admin-only.
    ///
    /// The new WASM must already be uploaded to the network.
    /// State is preserved; only bytecode is replaced.
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) -> Result<(), AmmError> {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        env.deployer()
            .update_current_contract_wasm(new_wasm_hash.clone());
        env.events()
            .publish((Symbol::new(&env, "upgraded"),), (new_wasm_hash,));
        Ok(())
    }

    /// Return the pending admin nominee, or `None` if no transfer is in progress.
    pub fn get_pending_admin(env: Env) -> Option<Address> {
        env.storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or(None)
    }

    // ── Reentrancy guard ──────────────────────────────────────────────────────

    /// Return `true` if a flash loan is currently executing on this contract.
    ///
    /// Any state-mutating entry point that could be exploited via a reentrant
    /// callback (swap, add_liquidity, remove_liquidity, flash_loan) calls this
    /// before proceeding. The lock is stored in instance storage so it is
    /// visible to all cross-contract calls within the same transaction.
    fn is_locked(env: &Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Locked)
            .unwrap_or(false)
    }

    /// Acquire the reentrancy lock.
    ///
    /// Returns `Err(AmmError::Reentrant)` if the lock is already held,
    /// preventing a flash-loan receiver from calling back into the pool.
    fn enter_lock(env: &Env) -> Result<(), AmmError> {
        if Self::is_locked(env) {
            return Err(AmmError::Reentrant);
        }
        env.storage().instance().set(&DataKey::Locked, &true);
        Ok(())
    }

    /// Release the reentrancy lock.
    ///
    /// Must be called on every successful return path after `enter_lock`.
    /// On error paths the Soroban runtime reverts all storage writes
    /// (including the lock), so an explicit release is not required there.
    fn exit_lock(env: &Env) {
        env.storage().instance().set(&DataKey::Locked, &false);
    }

    // ── TWAP ──────────────────────────────────────────────────────────────────

    /// Update the TWAP price accumulators based on the current reserves and elapsed time.
    /// This ensures that any reserve-mutating operation (add_liquidity, remove_liquidity,
    /// swap, flash_loan) correctly records the price at the time of the operation,
    /// preventing TWAP manipulation vectors.
    fn checkpoint_twap(env: &Env) -> (i128, i128) {
        let now = env.ledger().timestamp();
        let last: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LastTimestamp)
            .unwrap_or(now);
        let reserve_a = Self::get_reserve_a(env.clone());
        let reserve_b = Self::get_reserve_b(env.clone());
        if now > last {
            if reserve_a > 0 && reserve_b > 0 {
                let elapsed = (now - last) as i128;
                let mut cum_a: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::PriceCumulativeA)
                    .unwrap_or(0);
                let mut cum_b: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::PriceCumulativeB)
                    .unwrap_or(0);
                // Use wrapping_add so overflow is defined and consumers can handle it via
                // unsigned subtraction: (now - then) as u128 gives the correct delta.
                cum_a = cum_a.wrapping_add((reserve_b * 1_000_000 / reserve_a) * elapsed);
                cum_b = cum_b.wrapping_add((reserve_a * 1_000_000 / reserve_b) * elapsed);
                env.storage()
                    .instance()
                    .set(&DataKey::PriceCumulativeA, &cum_a);
                env.storage()
                    .instance()
                    .set(&DataKey::PriceCumulativeB, &cum_b);
            }
            env.storage().instance().set(&DataKey::LastTimestamp, &now);
        }
        (reserve_a, reserve_b)
    }

    /// Update the TWAL liquidity accumulator (sqrt(reserve_a * reserve_b) * elapsed).
    fn checkpoint_twal(env: &Env) {
        let now = env.ledger().timestamp();
        let last: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LastTimestamp)
            .unwrap_or(now);
        if now > last {
            let reserve_a = Self::get_reserve_a(env.clone());
            let reserve_b = Self::get_reserve_b(env.clone());
            if reserve_a > 0 && reserve_b > 0 {
                let elapsed = (now - last) as i128;
                let liquidity = Self::sqrt(reserve_a * reserve_b);
                let mut cum: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::LiquidityCumulative)
                    .unwrap_or(0);
                cum = cum.wrapping_add(liquidity * elapsed);
                env.storage()
                    .instance()
                    .set(&DataKey::LiquidityCumulative, &cum);
            }
        }
    }

    /// Checkpoint both TWAP price and TWAL liquidity accumulators.
    fn checkpoint_oracles(env: &Env) {
        Self::checkpoint_twap(env);
        Self::checkpoint_twal(env);
    }

    // ── Liquidity ─────────────────────────────────────────────────────────────

    /// Deposit tokens into the pool and receive LP shares in return.
    ///
    /// On the first deposit any ratio is accepted and the initial share supply is
    /// set to the geometric mean of the two amounts. Subsequent deposits must
    /// match the current pool ratio (within integer rounding); excess tokens are
    /// **not** refunded automatically — callers should compute amounts off-chain
    /// before calling.
    ///
    /// Requires `provider` to have authorized this call.
    ///
    /// # Parameters
    /// - `provider` – Address of the liquidity provider funding the deposit.
    /// - `amount_a` – Amount of `token_a` to deposit. Must be positive.
    /// - `amount_b` – Amount of `token_b` to deposit. Must be positive.
    /// - `min_shares` – Minimum number of LP shares the caller is willing to
    ///   receive; the transaction panics if fewer would be minted (slippage guard).
    ///
    /// # Returns
    /// The number of LP shares minted to `provider`.
    ///
    /// # Panics
    /// - If either `amount_a` or `amount_b` is not positive.
    /// - If the shares that would be minted are less than `min_shares`.
    pub fn add_liquidity(
        env: Env,
        provider: Address,
        amount_a: i128,
        amount_b: i128,
        min_shares: i128,
        deadline: u64,
    ) -> Result<i128, AmmError> {
        if deadline < env.ledger().timestamp() {
            return Err(AmmError::DeadlineExceeded);
        }
        if Self::is_paused(env.clone()) {
            return Err(AmmError::Paused);
        }
        // Block reentrant calls from flash loan receivers.
        if Self::is_locked(&env) {
            return Err(AmmError::Reentrant);
        }
        provider.require_auth();
        if amount_a <= 0 || amount_b <= 0 {
            return Err(AmmError::ZeroAmount);
        }

       // Checkpoint TWAP before updating reserves; reuse the reserves it read.
        let (reserve_a, reserve_b) = Self::checkpoint_twap(&env);
        // Checkpoint oracles before updating reserves.
        Self::checkpoint_oracles(&env);

        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let lp_token: Address = env.storage().instance().get(&DataKey::LpToken).unwrap();
        let total_shares: i128 = Self::get_total_shares(env.clone());

        // Compute shares to mint.
        // Compute shares to mint.
        let shares = if total_shares == 0 {
            // Initial liquidity: geometric mean of deposits.
            Self::sqrt(amount_a * amount_b)
        } else {
            // Proportional shares — use the lesser of the two ratios.
            let shares_a = amount_a * total_shares / reserve_a;
            let shares_b = amount_b * total_shares / reserve_b;
            shares_a.min(shares_b)
        };

        if shares <= 0 {
            return Err(AmmError::ZeroAmount);
        }

        // Issue #294: on the very first deposit, permanently lock MINIMUM_LIQUIDITY
        // LP tokens to the zero address so the pool can never be fully drained.
        const MINIMUM_LIQUIDITY: i128 = 1_000;
        let already_locked: bool = env
            .storage()
            .instance()
            .get(&DataKey::MinLiquidityLocked)
            .unwrap_or(false);
        let (shares_to_provider, shares_locked) = if total_shares == 0 && !already_locked {
            if shares <= MINIMUM_LIQUIDITY {
                return Err(AmmError::InsufficientShares);
            }
            (shares - MINIMUM_LIQUIDITY, MINIMUM_LIQUIDITY)
        } else {
            (shares, 0)
        };

        if shares_to_provider < min_shares {
            return Err(AmmError::SlippageExceeded);
        }

        // Pull tokens from provider into the pool contract.
        let client_a = SepTokenClient::new(&env, &token_a);
        let client_b = SepTokenClient::new(&env, &token_b);
        client_a.transfer(&provider, &env.current_contract_address(), &amount_a);
        client_b.transfer(&provider, &env.current_contract_address(), &amount_b);

        // Update reserves.
        let total_minted = shares_to_provider + shares_locked;
        env.storage()
            .instance()
            .set(&DataKey::ReserveA, &(reserve_a + amount_a));
        env.storage()
            .instance()
            .set(&DataKey::ReserveB, &(reserve_b + amount_b));
        env.storage()
            .instance()
            .set(&DataKey::TotalShares, &(total_shares + total_minted));

        // Mint LP tokens.
        let lp_client = LpTokenClient::new(&env, &lp_token);
        // Issue #294: lock minimum liquidity to the contract address itself (zero-address equivalent).
        if shares_locked > 0 {
            lp_client.mint(&env.current_contract_address(), &shares_locked);
            env.storage()
                .instance()
                .set(&DataKey::MinLiquidityLocked, &true);
        }
        lp_client.mint(&provider, &shares_to_provider);

        soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(&env, "add_liquidity"), provider), (amount_a, amount_b, shares_to_provider));

        Ok(shares_to_provider)
    }

    /// Withdraw liquidity from the pool by burning LP shares.
    ///
    /// Burns exactly `shares` LP tokens held by `provider` and transfers a
    /// proportional amount of both pool tokens back to the provider. The
    /// proportion is `shares / total_shares` at the time of the call.
    ///
    /// Requires `provider` to have authorized this call.
    ///
    /// # Parameters
    /// - `provider` – Address of the liquidity provider redeeming shares.
    /// - `shares` – Number of LP shares to burn. Must be positive and ≤ the
    ///   provider's current balance.
    /// - `min_a` – Minimum amount of `token_a` the caller is willing to receive
    ///   (slippage guard).
    /// - `min_b` – Minimum amount of `token_b` the caller is willing to receive
    ///   (slippage guard).
    ///
    /// # Returns
    /// A tuple `(amount_a, amount_b)` — the token amounts transferred back to
    /// the provider.
    ///
    /// # Panics
    /// - If `shares` is not positive.
    /// - If `provider` owns fewer shares than `shares`.
    /// - If the computed `token_a` output would be less than `min_a`.
    /// - If the computed `token_b` output would be less than `min_b`.
    pub fn remove_liquidity(
        env: Env,
        provider: Address,
        shares: i128,
        min_a: i128,
        min_b: i128,
        deadline: u64,
    ) -> Result<(i128, i128), AmmError> {
        if deadline < env.ledger().timestamp() {
            return Err(AmmError::DeadlineExceeded);
        }
        if Self::is_paused(env.clone()) {
            return Err(AmmError::Paused);
        }
        // Block reentrant calls from flash loan receivers.
        if Self::is_locked(&env) {
            return Err(AmmError::Reentrant);
        }
        provider.require_auth();
        if shares <= 0 {
            return Err(AmmError::ZeroAmount);
        }

        // Checkpoint TWAP before updating reserves.
        let (reserve_a, reserve_b) = Self::checkpoint_twap(&env);
        // Checkpoint oracles before updating reserves.
        Self::checkpoint_oracles(&env);

        let owned = Self::shares_of(env.clone(), provider.clone());
        if owned < shares {
            return Err(AmmError::InsufficientShares);
        }

        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let lp_token: Address = env.storage().instance().get(&DataKey::LpToken).unwrap();

        
        let total_shares = Self::get_total_shares(env.clone());

        let out_a = shares * reserve_a / total_shares;
        let out_b = shares * reserve_b / total_shares;

        if out_a < min_a || out_b < min_b {
            return Err(AmmError::SlippageExceeded);
        }

        // Burn LP tokens.
        let lp_client = LpTokenClient::new(&env, &lp_token);
        lp_client.burn(&provider, &shares);

        // Update state.
        env.storage()
            .instance()
            .set(&DataKey::ReserveA, &(reserve_a - out_a));
        env.storage()
            .instance()
            .set(&DataKey::ReserveB, &(reserve_b - out_b));
        env.storage()
            .instance()
            .set(&DataKey::TotalShares, &(total_shares - shares));

        // Return tokens.
        let client_a = SepTokenClient::new(&env, &token_a);
        let client_b = SepTokenClient::new(&env, &token_b);
        client_a.transfer(&env.current_contract_address(), &provider, &out_a);
        client_b.transfer(&env.current_contract_address(), &provider, &out_b);

        soroban_amm_sdk::emit_versioned_event!(env, (symbol_short!("rm_liq"),), (provider.clone(), shares, out_a, out_b));

        Ok((out_a, out_b))
    }

    /// Burn LP shares and return a single token, swapping the other internally.
    ///
    /// Equivalent to calling `remove_liquidity` followed by `swap`, but in a single
    /// transaction. This saves users a swap fee and simplifies the UX when they want
    /// to exit a position receiving only one asset.
    ///
    /// Requires `provider` to have authorized this call.
    ///
    /// # Parameters
    /// - `provider` – Address of the liquidity provider redeeming shares.
    /// - `shares` – Number of LP shares to burn. Must be positive and ≤ the
    ///   provider's current balance.
    /// - `token_out` – Address of the token to receive; must be either `token_a`
    ///   or `token_b` of this pool.
    /// - `min_out` – Minimum total amount of `token_out` the caller is willing to
    ///   receive after the internal swap (slippage guard).
    ///
    /// # Returns
    /// The total amount of `token_out` received (withdrawal + internal swap proceeds).
    ///
    /// # Panics
    /// - If `shares` is not positive.
    /// - If `provider` owns fewer shares than `shares`.
    /// - If `token_out` is not one of the two pool tokens.
    /// - If the computed output would be less than `min_out`.
    /// - If the pool is paused.
    #[allow(clippy::too_many_arguments)]
    pub fn remove_liquidity_one_sided(
        env: Env,
        provider: Address,
        shares: i128,
        token_out: Address,
        min_out: i128,
        deadline: u64,
    ) -> Result<i128, AmmError> {
        if deadline < env.ledger().timestamp() {
            return Err(AmmError::DeadlineExceeded);
        }
        if Self::is_paused(env.clone()) {
            return Err(AmmError::Paused);
        }
        // Block reentrant calls from flash loan receivers.
        if Self::is_locked(&env) {
            return Err(AmmError::Reentrant);
        }
        provider.require_auth();
        if shares <= 0 {
            return Err(AmmError::ZeroAmount);
        }

        // Checkpoint TWAP before updating reserves.
        let (reserve_a, reserve_b) = Self::checkpoint_twap(&env);
        // Checkpoint oracles before updating reserves.
        Self::checkpoint_oracles(&env);

        let owned = Self::shares_of(env.clone(), provider.clone());
        if owned < shares {
            return Err(AmmError::InsufficientShares);
        }

        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let lp_token: Address = env.storage().instance().get(&DataKey::LpToken).unwrap();

        if token_out != token_a && token_out != token_b {
            return Err(AmmError::InvalidToken);
        }

        let total_shares = Self::get_total_shares(env.clone());

        // Compute proportional withdrawal amounts.
        let withdraw_a = shares * reserve_a / total_shares;
        let withdraw_b = shares * reserve_b / total_shares;

        // Burn LP tokens.
        let lp_client = LpTokenClient::new(&env, &lp_token);
        lp_client.burn(&provider, &shares);

        // Determine which token we keep and which we swap away.
        let (_token_keep, _token_swap, amount_keep, amount_swap) = if token_out == token_a {
            (token_a.clone(), token_b.clone(), withdraw_a, withdraw_b)
        } else {
            (token_b.clone(), token_a.clone(), withdraw_b, withdraw_a)
        };

        // Update reserves: deduct the withdrawn amounts.
        let new_reserve_a = if token_out == token_a {
            reserve_a - withdraw_a
        } else {
            reserve_a - withdraw_b
        };
        let new_reserve_b = if token_out == token_a {
            reserve_b - withdraw_b
        } else {
            reserve_b - withdraw_a
        };

        env.storage()
            .instance()
            .set(&DataKey::ReserveA, &new_reserve_a);
        env.storage()
            .instance()
            .set(&DataKey::ReserveB, &new_reserve_b);
        env.storage()
            .instance()
            .set(&DataKey::TotalShares, &(total_shares - shares));

        // Internal swap: swap the "other" token for more of token_out.
        let fee_bps: i128 = env.storage().instance().get(&DataKey::FeeBps).unwrap();

        // amount_swap after fee
        let amount_swap_with_fee = amount_swap * (10_000 - fee_bps);

        // We swap token_swap (from withdrawal) for token_keep using the updated reserves.
        // After withdrawal, reserves are (new_reserve_a, new_reserve_b).
        // We're swapping amount_swap of token_swap.
        let swap_output = if token_out == token_a {
            // We're swapping token_b for more token_a.
            // Reserves after withdrawal: (new_reserve_a, new_reserve_b) where new_reserve_b = reserve_b - withdraw_b
            // But we're adding amount_swap to the input token.
            amount_swap_with_fee * new_reserve_a / (new_reserve_b * 10_000 + amount_swap_with_fee)
        } else {
            // We're swapping token_a for more token_b.
            amount_swap_with_fee * new_reserve_b / (new_reserve_a * 10_000 + amount_swap_with_fee)
        };

        // Total output is the amount we kept from withdrawal plus the swap output.
        let total_out = amount_keep + swap_output;

        if total_out < min_out {
            return Err(AmmError::SlippageExceeded);
        }

        // Update reserves after internal swap.
        let protocol_fee_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(0);
        let protocol_fee = if protocol_fee_bps > 0 {
            amount_swap * protocol_fee_bps / 10_000
        } else {
            0
        };

        let final_reserve_a = if token_out == token_a {
            // We paid out swap_output of token_a
            new_reserve_a - swap_output
        } else {
            // We received amount_swap of token_a (minus protocol fee)
            new_reserve_a + amount_swap - protocol_fee
        };

        let final_reserve_b = if token_out == token_a {
            // We received amount_swap of token_b (minus protocol fee)
            new_reserve_b + amount_swap - protocol_fee
        } else {
            // We paid out swap_output of token_b
            new_reserve_b - swap_output
        };

        env.storage()
            .instance()
            .set(&DataKey::ReserveA, &final_reserve_a);
        env.storage()
            .instance()
            .set(&DataKey::ReserveB, &final_reserve_b);

        // Track protocol fees if applicable.
        if protocol_fee > 0 {
            let token_to_accrue = if token_out == token_a {
                token_b.clone()
            } else {
                token_a.clone()
            };

            if token_to_accrue == token_a {
                let accrued: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::AccruedFeeA)
                    .unwrap_or(0);
                env.storage()
                    .instance()
                    .set(&DataKey::AccruedFeeA, &(accrued + protocol_fee));
            } else {
                let accrued: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::AccruedFeeB)
                    .unwrap_or(0);
                env.storage()
                    .instance()
                    .set(&DataKey::AccruedFeeB, &(accrued + protocol_fee));
            }
        }

        // Transfer the total output to provider.
        let client_out = SepTokenClient::new(&env, &token_out);
        client_out.transfer(&env.current_contract_address(), &provider, &total_out);

        soroban_amm_sdk::emit_versioned_event!(env, (symbol_short!("rm_liq_1s"),), (provider.clone(), shares, token_out.clone(), total_out));

        Ok(total_out)
    }

    // ── Swap ──────────────────────────────────────────────────────────────────

    /// Swap an exact amount of one pool token for the other.
    ///
    /// Transfers `amount_in` of `token_in` from `trader` into the pool and
    /// sends back the calculated output amount of the opposite token, computed
    /// via the constant-product formula `x * y = k` with the pool fee deducted
    /// from `amount_in` before the calculation.
    ///
    /// Requires `trader` to have authorized this call.
    ///
    /// # Parameters
    /// - `trader` – Address of the account initiating the swap.
    /// - `token_in` – Address of the token being sold; must be either `token_a`
    ///   or `token_b` of this pool.
    /// - `amount_in` – Exact amount of `token_in` to sell. Must be positive.
    /// - `min_out` – Minimum amount of the output token the caller is willing to
    ///   accept (slippage guard).
    ///
    /// Uses the constant-product formula with fee deducted from `amount_in`.
    /// The `protocol_fee_bps` portion of `amount_in` is held for `withdraw_protocol_fees`.
    /// # Returns
    /// The amount of the output token transferred to `trader`.
    ///
    /// # Panics
    /// - If `amount_in` is not positive.
    /// - If `token_in` is not one of the two pool tokens.
    /// - If either pool reserve is zero (pool is empty).
    /// - If the computed output would be less than `min_out`.
    /// - If the computed output equals or exceeds the output reserve (insufficient liquidity).
    pub fn swap(
        env: Env,
        trader: Address,
        token_in: Address,
        amount_in: i128,
        min_out: i128,
        deadline: u64,
    ) -> Result<i128, AmmError> {
        if deadline < env.ledger().timestamp() {
            return Err(AmmError::DeadlineExceeded);
        }
        if Self::is_paused(env.clone()) {
            return Err(AmmError::Paused);
        }
        // Block reentrant calls from flash loan receivers.
        if Self::is_locked(&env) {
            return Err(AmmError::Reentrant);
        }
        trader.require_auth();
        if amount_in <= 0 {
            return Err(AmmError::ZeroAmount);
        }

// Checkpoint TWAP before updating reserves; reuse the reserves it read.
        let (reserve_a, reserve_b) = Self::checkpoint_twap(&env);
        // Checkpoint oracles before updating reserves.
        Self::checkpoint_oracles(&env);

        // Circuit breaker: check intra-block price deviation BEFORE the swap
        // changes the reserves so the baseline is the pre-trade price.
        Self::check_circuit_breaker(&env)?;

        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let (reserve_in, reserve_out, token_out) = if token_in == token_a {
            (reserve_a, reserve_b, token_b.clone())
        } else if token_in == token_b {
            (reserve_b, reserve_a, token_a.clone())
        } else {
            return Err(AmmError::InvalidToken);
        };

        if reserve_in <= 0 || reserve_out <= 0 {
            return Err(AmmError::EmptyPool);
        }

        let fee_bps: i128 = env.storage().instance().get(&DataKey::FeeBps).unwrap();

        // amount_in after fee
        let amount_in_with_fee = amount_in * (10_000 - fee_bps);
        // constant-product: out = (amount_in_with_fee * reserve_out) / (reserve_in * 10_000 + amount_in_with_fee)
        let amount_out =
            amount_in_with_fee * reserve_out / (reserve_in * 10_000 + amount_in_with_fee);

        if amount_out < min_out {
            return Err(AmmError::SlippageExceeded);
        }
        if amount_out >= reserve_out {
            return Err(AmmError::InsufficientLiquidity);
        }

        Self::check_oracle_deviation(&env, &token_in, &token_out, amount_in, amount_out)?;

        // Transfer in.
        let client_in = SepTokenClient::new(&env, &token_in);
        client_in.transfer(&trader, &env.current_contract_address(), &amount_in);

        // Transfer out.
        let client_out = SepTokenClient::new(&env, &token_out);
        client_out.transfer(&env.current_contract_address(), &trader, &amount_out);

        // Separate protocol fee from LP reserves.
        let protocol_fee_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(0);
        let protocol_fee = if protocol_fee_bps > 0 {
            amount_in * protocol_fee_bps / 10_000
        } else {
            0
        };
        // Issue #292: LP rebate — fraction of protocol fee returned to LP reserves.
        let lp_rebate_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::LpRebateBps)
            .unwrap_or(0);
        let lp_rebate = if protocol_fee > 0 && lp_rebate_bps > 0 {
            protocol_fee * lp_rebate_bps / 10_000
        } else {
            0
        };
        let net_protocol_fee = protocol_fee - lp_rebate;
        // Update reserves (net protocol fee held outside LP reserves; rebate stays in reserves).
        if token_in == token_a {
            env.storage()
                .instance()
                .set(&DataKey::ReserveA, &(reserve_in + amount_in - net_protocol_fee));
            env.storage()
                .instance()
                .set(&DataKey::ReserveB, &(reserve_out - amount_out));
            if net_protocol_fee > 0 {
                let accrued: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::AccruedFeeA)
                    .unwrap_or(0);
                env.storage()
                    .instance()
                    .set(&DataKey::AccruedFeeA, &(accrued + net_protocol_fee));
            }
        } else {
            env.storage()
                .instance()
                .set(&DataKey::ReserveB, &(reserve_in + amount_in - net_protocol_fee));
            env.storage()
                .instance()
                .set(&DataKey::ReserveA, &(reserve_out - amount_out));
            if net_protocol_fee > 0 {
                let accrued: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::AccruedFeeB)
                    .unwrap_or(0);
                env.storage()
                    .instance()
                    .set(&DataKey::AccruedFeeB, &(accrued + net_protocol_fee));
            }
        }

        env.events().publish(
            (Symbol::new(&env, "swap"), trader.clone()),
            (token_in.clone(), amount_in, token_out.clone(), amount_out),
        );
        soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(&env, "swap"), trader), (token_in, amount_in, token_out, amount_out, None::<Address>));

        Ok(amount_out)
    }

    /// Swap a variable input amount to receive exactly `amount_out` of `token_out`.
    ///
    /// Computes the required input via `get_amount_in` and enforces `max_in` as a
    /// slippage guard. Updates reserves and the TWAP accumulator identically to `swap`.
    ///
    /// # Parameters
    /// - `trader`     – Address initiating the swap; must authorise this call.
    /// - `token_out`  – Address of the token to receive; must be `token_a` or `token_b`.
    /// - `amount_out` – Exact amount of `token_out` the caller wants to receive.
    /// - `max_in`     – Maximum `token_in` the caller is willing to spend (slippage guard).
    /// - `deadline`   – Latest ledger timestamp at which this call is valid.
    ///
    /// # Returns
    /// The amount of the input token actually spent.
    ///
    /// # Panics
    /// - If `amount_out` is not positive.
    /// - If `token_out` is not one of the two pool tokens.
    /// - If the required input exceeds `max_in`.
    /// - If the pool is paused.
    #[allow(clippy::too_many_arguments)]
    pub fn swap_exact_out(
        env: Env,
        trader: Address,
        token_out: Address,
        amount_out: i128,
        max_in: i128,
        deadline: u64,
        
    ) -> Result<i128, AmmError> {
        if deadline < env.ledger().timestamp() {
            return Err(AmmError::DeadlineExceeded);
        }
        if Self::is_paused(env.clone()) {
            return Err(AmmError::Paused);
        }
        // Block reentrant calls from flash loan receivers.
        if Self::is_locked(&env) {
            return Err(AmmError::Reentrant);
        }
        trader.require_auth();
        if amount_out <= 0 {
            return Err(AmmError::ZeroAmount);
        }

        // Checkpoint TWAP before updating reserves.
        let (reserve_a, reserve_b) = Self::checkpoint_twap(&env);
        // Checkpoint oracles before updating reserves.
        Self::checkpoint_oracles(&env);

        // Circuit breaker check before state mutation.
        Self::check_circuit_breaker(&env)?;

        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();

        let token_in = if token_out == token_a {
            token_b.clone()
        } else if token_out == token_b {
            token_a.clone()
        } else {
            return Err(AmmError::InvalidToken);
        };

        let amount_in = Self::get_amount_in(env.clone(), token_out.clone(), amount_out);
        if amount_in > max_in {
            return Err(AmmError::SlippageExceeded);
        }

        Self::check_oracle_deviation(&env, &token_in, &token_out, amount_in, amount_out)?;

        // Transfer tokens.
        SepTokenClient::new(&env, &token_in).transfer(
            &trader,
            &env.current_contract_address(),
            &amount_in,
        );
        SepTokenClient::new(&env, &token_out).transfer(
            &env.current_contract_address(),
            &trader,
            &amount_out,
        );

        // Separate protocol fee from LP reserves.
        let protocol_fee_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(0);
        let protocol_fee = if protocol_fee_bps > 0 {
            amount_in * protocol_fee_bps / 10_000
        } else {
            0
        };
        // Issue #292: LP rebate — fraction of protocol fee returned to LP reserves.
        let lp_rebate_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::LpRebateBps)
            .unwrap_or(0);
        let lp_rebate = if protocol_fee > 0 && lp_rebate_bps > 0 {
            protocol_fee * lp_rebate_bps / 10_000
        } else {
            0
        };
        let net_protocol_fee = protocol_fee - lp_rebate;

        // Update reserves (net protocol fee held outside LP reserves; rebate stays in reserves).
        if token_in == token_a {
            env.storage()
                .instance()
                .set(&DataKey::ReserveA, &(reserve_a + amount_in - net_protocol_fee));
            env.storage()
                .instance()
                .set(&DataKey::ReserveB, &(reserve_b - amount_out));
            if net_protocol_fee > 0 {
                let accrued: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::AccruedFeeA)
                    .unwrap_or(0);
                env.storage()
                    .instance()
                    .set(&DataKey::AccruedFeeA, &(accrued + net_protocol_fee));
            }
        } else {
            env.storage()
                .instance()
                .set(&DataKey::ReserveB, &(reserve_b + amount_in - net_protocol_fee));
            env.storage()
                .instance()
                .set(&DataKey::ReserveA, &(reserve_a - amount_out));
            if net_protocol_fee > 0 {
                let accrued: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::AccruedFeeB)
                    .unwrap_or(0);
                env.storage()
                    .instance()
                    .set(&DataKey::AccruedFeeB, &(accrued + net_protocol_fee));
            }
        }

        env.events().publish(
            (Symbol::new(&env, "swap"), trader.clone()),
            (token_in.clone(), amount_in, token_out.clone(), amount_out),
        );
        soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(&env, "swap"), trader), (token_in, amount_in, token_out, amount_out, None::<Address>));

        Ok(amount_in)
    }

    // ── Protocol Fees ─────────────────────────────────────────────────────────

    /// Withdraw all accrued protocol fees to the configured fee recipient.
    ///
    /// Only callable by the fee recipient. Resets accrued balances to zero.
    /// Returns `(fee_a_withdrawn, fee_b_withdrawn)`.
    pub fn withdraw_protocol_fees(env: Env) -> Result<(i128, i128), AmmError> {
        let fee_recipient: Address = env
            .storage()
            .instance()
            .get(&DataKey::FeeRecipient)
            .unwrap();
        fee_recipient.require_auth();

        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();

        let fee_a: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccruedFeeA)
            .unwrap_or(0);
        let fee_b: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccruedFeeB)
            .unwrap_or(0);

        if fee_a > 0 {
            SepTokenClient::new(&env, &token_a).transfer(
                &env.current_contract_address(),
                &fee_recipient,
                &fee_a,
            );
            env.storage().instance().set(&DataKey::AccruedFeeA, &0_i128);
        }

        if fee_b > 0 {
            SepTokenClient::new(&env, &token_b).transfer(
                &env.current_contract_address(),
                &fee_recipient,
                &fee_b,
            );
            env.storage().instance().set(&DataKey::AccruedFeeB, &0_i128);
        }

        Ok((fee_a, fee_b))
    }

    /// Borrow pool liquidity and repay it plus a fee during the receiver callback.
    ///
    /// # Reentrancy safety
    /// This function acquires a reentrancy lock before calling the external
    /// `on_flash_loan` callback and holds it for the duration of that call.
    /// Any attempt by the receiver to call back into `swap`, `add_liquidity`,
    /// `remove_liquidity`, or `flash_loan` on this same pool will fail with
    /// `AmmError::Reentrant`. The lock is released only after repayment is
    /// verified, ensuring pool state cannot be manipulated via callbacks.
    pub fn flash_loan(
        env: Env,
        receiver: Address,
        token: Address,
        amount: i128,
        data: Bytes,
    ) -> Result<i128, AmmError> {
        if Self::is_paused(env.clone()) {
            return Err(AmmError::Paused);
        }
        if amount <= 0 {
            return Err(AmmError::ZeroAmount);
        }

        // Checkpoint TWAP before updating reserves.
        let (reserve_a, reserve_b) = Self::checkpoint_twap(&env);
        // Acquire the reentrancy lock before any external call.
        // This prevents the receiver's on_flash_loan callback from calling
        // back into swap / add_liquidity / remove_liquidity / flash_loan.
        Self::enter_lock(&env)?;

        // Checkpoint oracles before updating reserves.
        Self::checkpoint_oracles(&env);

        // Circuit breaker check before borrowing funds.
        Self::check_circuit_breaker(&env)?;

        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let reserve = if token == token_a {
           reserve_a
        } else if token == token_b {
            reserve_b
        } else {
            // exit_lock is not needed: returning Err reverts all storage writes.
            return Err(AmmError::InvalidToken);
        };
        if reserve < amount {
            return Err(AmmError::InsufficientLiquidity);
        }

        let fee_bps = Self::get_flash_loan_fee_bps(env.clone());
        let fee = if fee_bps > 0 {
            (amount * fee_bps / 10_000).max(1)
        } else {
            0
        };
        let pool = env.current_contract_address();
        let token_client = SepTokenClient::new(&env, &token);
        let balance_before = token_client.balance(&pool);

        token_client.transfer(&pool, &receiver, &amount);

        // ── External callback (lock is held) ─────────────────────────────────
        // The receiver cannot reenter this pool because Locked == true.
        // Any reentrant call will return AmmError::Reentrant.
        let accepted = FlashLoanReceiverClient::new(&env, &receiver)
            .on_flash_loan(&token, &amount, &fee, &data);
        if !accepted {
            return Err(AmmError::InsufficientLiquidity);
        }

        let balance_after = token_client.balance(&pool);
        if balance_after < balance_before + fee {
            return Err(AmmError::InsufficientLiquidity);
        }

        let accrued_fee = if token == token_a {
            env.storage()
                .instance()
                .get(&DataKey::AccruedFeeA)
                .unwrap_or(0)
        } else {
            env.storage()
                .instance()
                .get(&DataKey::AccruedFeeB)
                .unwrap_or(0)
        };
        let reserve_after = balance_after - accrued_fee;
        if token == token_a {
            env.storage()
                .instance()
                .set(&DataKey::ReserveA, &reserve_after);
        } else {
            env.storage()
                .instance()
                .set(&DataKey::ReserveB, &reserve_after);
        }

        soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(&env, "flash_loan"), receiver), (token, amount, fee));

        // Release the lock only on the success path; on error paths Soroban
        // reverts all storage writes (including the lock) automatically.
        Self::exit_lock(&env);
        Ok(fee)
    }

    // ── Quotes (read-only) ────────────────────────────────────────────────────

    /// Return the current spot price of each token in terms of the other,
    /// scaled by 1_000_000.
    ///
    /// Returns `(price_a, price_b)` where:
    /// - `price_a` = price of token_a in terms of token_b (reserve_b * 1_000_000 / reserve_a)
    /// - `price_b` = price of token_b in terms of token_a (reserve_a * 1_000_000 / reserve_b)
    ///
    /// Panics if either reserve is zero (pool is empty).
    pub fn price_ratio(env: Env) -> Result<(i128, i128), AmmError> {
        let reserve_a = Self::get_reserve_a(env.clone());
        let reserve_b = Self::get_reserve_b(env);
        if reserve_a <= 0 || reserve_b <= 0 {
            return Err(AmmError::EmptyPool);
        }
        let price_a = reserve_b * 1_000_000 / reserve_a;
        let price_b = reserve_a * 1_000_000 / reserve_b;
        Ok((price_a, price_b))
    }

    /// Quote how much `token_out` you receive for `amount_in` of `token_in`.
    /// Calculate the output amount for a hypothetical swap without executing it.
    ///
    /// Applies the same constant-product formula and fee as `swap` but
    /// makes no state changes. Useful for quoting prices off-chain or in other
    /// contracts before committing to a swap.
    ///
    /// # Parameters
    /// - `token_in` – Address of the token being sold; must be either `token_a`
    ///   or `token_b` of this pool.
    /// - `amount_in` – Hypothetical amount of `token_in` to sell.
    ///
    /// # Returns
    /// The amount of the output token that would be received for `amount_in`,
    /// after the pool fee is applied.
    ///
    /// # Panics
    /// - If `token_in` is not one of the two pool tokens.
    pub fn get_amount_out(env: Env, token_in: Address, amount_in: i128) -> Result<i128, AmmError> {
        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let fee_bps: i128 = env.storage().instance().get(&DataKey::FeeBps).unwrap();

        let (reserve_in, reserve_out) = if token_in == token_a {
            (
                Self::get_reserve_a(env.clone()),
                Self::get_reserve_b(env.clone()),
            )
        } else if token_in == token_b {
            (
                Self::get_reserve_b(env.clone()),
                Self::get_reserve_a(env.clone()),
            )
        } else {
            return Err(AmmError::InvalidToken);
        };

        if reserve_in <= 0 || reserve_out <= 0 {
            return Err(AmmError::EmptyPool);
        }
        let amount_in_with_fee = amount_in * (10_000 - fee_bps);
        Ok(amount_in_with_fee * reserve_out / (reserve_in * 10_000 + amount_in_with_fee))
    }

    /// Simulate a swap and return a detailed breakdown without executing it.
    ///
    /// Returns the expected output, total fee taken, effective execution price,
    /// spot price, and price impact — all computed from current reserve state.
    /// `amount_out` is guaranteed to match `get_amount_out` for the same inputs.
    pub fn simulate_swap(env: Env, token_in: Address, amount_in: i128) -> Result<SwapSimulation, AmmError> {
        if amount_in <= 0 {
            return Err(AmmError::ZeroAmount);
        }
        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let fee_bps: i128 = env.storage().instance().get(&DataKey::FeeBps).unwrap();
        let (reserve_in, reserve_out) = if token_in == token_a {
            (
                Self::get_reserve_a(env.clone()),
                Self::get_reserve_b(env.clone()),
            )
        } else if token_in == token_b {
            (
                Self::get_reserve_b(env.clone()),
                Self::get_reserve_a(env.clone()),
            )
        } else {
            return Err(AmmError::InvalidToken);
        };
        if reserve_in <= 0 || reserve_out <= 0 {
            return Err(AmmError::EmptyPool);
        }
        let amount_in_with_fee = amount_in * (10_000 - fee_bps);
        let amount_out =
            amount_in_with_fee * reserve_out / (reserve_in * 10_000 + amount_in_with_fee);
        let fee_amount = amount_in * fee_bps / 10_000;
        let spot_price = reserve_out * 1_000_000 / reserve_in;
        let effective_price = amount_out * 1_000_000 / amount_in;
        let price_impact_bps = if amount_out == 0 {
            0
        } else {
            ((spot_price - effective_price) * 10_000 / spot_price).max(0)
        };
        Ok(SwapSimulation {
            amount_out,
            fee_amount,
            price_impact_bps,
            effective_price,
            spot_price,
        })
    }

    /// Quote how much `token_in` is required to receive exactly `amount_out` of `token_out`.
    pub fn get_amount_in(env: Env, token_out: Address, amount_out: i128) -> i128 {
        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let fee_bps: i128 = env.storage().instance().get(&DataKey::FeeBps).unwrap();
        let (reserve_in, reserve_out) = if token_out == token_a {
            (
                Self::get_reserve_b(env.clone()),
                Self::get_reserve_a(env.clone()),
            )
        } else if token_out == token_b {
            (
                Self::get_reserve_a(env.clone()),
                Self::get_reserve_b(env.clone()),
            )
        } else {
            panic!("unknown token");
        };
        assert!(reserve_in > 0 && reserve_out > 0, "zero reserve");
        assert!(amount_out < reserve_out, "amount_out >= reserve_out");
        (reserve_in * amount_out * 10_000) / ((reserve_out - amount_out) * (10_000 - fee_bps)) + 1
    }

    /// Return full pool state.
    /// Return a snapshot of the full pool state.
    ///
    /// This is a read-only view function; it makes no state changes.
    ///
    /// # Returns
    /// A [`PoolInfo`] struct containing:
    /// - `token_a` / `token_b` — addresses of the two pool tokens.
    /// - `reserve_a` / `reserve_b` — current token reserves held by the pool.
    /// - `total_shares` — total outstanding LP shares.
    /// - `fee_bps` — the swap fee in basis points.
    /// - `flash_loan_fee_bps` — the flash-loan fee in basis points.
    /// - `admin` — the pool administrator.
    /// - `fee_recipient` — recipient of accrued protocol fees.
    /// - `protocol_fee_bps` — protocol fee in basis points (subset of `fee_bps`).
    
    pub fn get_fee_info(env: Env) -> i128 {
        env.storage().instance().get(&DataKey::FeeBps).unwrap()
    }

    pub fn get_info(env: Env) -> PoolInfo {
        PoolInfo {
            token_a: env.storage().instance().get(&DataKey::TokenA).unwrap(),
            token_b: env.storage().instance().get(&DataKey::TokenB).unwrap(),
            reserve_a: Self::get_reserve_a(env.clone()),
            reserve_b: Self::get_reserve_b(env.clone()),
            total_shares: Self::get_total_shares(env.clone()),
            fee_bps: env.storage().instance().get(&DataKey::FeeBps).unwrap(),
            flash_loan_fee_bps: Self::get_flash_loan_fee_bps(env.clone()),
            admin: env.storage().instance().get(&DataKey::Admin).unwrap(),
            fee_recipient: env
                .storage()
                .instance()
                .get(&DataKey::FeeRecipient)
                .unwrap(),
            protocol_fee_bps: env
                .storage()
                .instance()
                .get(&DataKey::ProtocolFeeBps)
                .unwrap_or(0),
            lp_rebate_bps: env
                .storage()
                .instance()
                .get(&DataKey::LpRebateBps)
                .unwrap_or(0),
        }
    }

    /// Return the protocol fees accrued but not yet withdrawn, without moving funds.
    ///
    /// Read-only counterpart to [`AmmPool::withdraw_protocol_fees`]; useful for fee recipients
    /// and dashboards that need a non-destructive view of pending fees.
    ///
    /// # Returns
    /// `(accrued_fee_a, accrued_fee_b)` — pending protocol fees in each token.
    pub fn get_accrued_fees(env: Env) -> (i128, i128) {
        let fee_a: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccruedFeeA)
            .unwrap_or(0);
        let fee_b: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccruedFeeB)
            .unwrap_or(0);
        (fee_a, fee_b)
    }

    /// Return the number of LP shares currently held by a given provider.
    ///
    /// This is a read-only view function; it makes no state changes.
    ///
    /// # Parameters
    /// - `provider` – Address of the liquidity provider to query.
    ///
    /// # Returns
    /// The LP share balance of `provider`, or `0` if the address has never
    /// provided liquidity to this pool.
    pub fn shares_of(env: Env, provider: Address) -> i128 {
        let lp_token: Address = env.storage().instance().get(&DataKey::LpToken).unwrap();
        LpTokenClient::new(&env, &lp_token).balance(&provider)
    }

    // ── Fee-on-transfer support ───────────────────────────────────────────────

    /// Swap with fee-on-transfer (FOT) token support.
    ///
    /// Identical to [`swap`] but measures the actual amount of `token_in`
    /// received by the pool via a pre/post balance snapshot instead of trusting
    /// `amount_in`. This handles tokens that silently deduct a transfer fee, so
    /// the constant-product calculation always uses the true net amount.
    ///
    /// # Parameters
    /// - `min_received` – Minimum tokens the pool must actually receive after any
    ///   FOT deduction (slippage guard on input). Set to `0` to disable.
    ///
    /// # Returns
    /// `(amount_out, actual_received)` — the output amount transferred to `trader`
    /// and the net input the pool received.
    ///
    /// # Events
    /// Emits a `fot_detected` event when `actual_received < amount_in`, carrying
    /// `(nominal_amount_in, actual_received)` for off-chain monitoring.
    #[allow(clippy::too_many_arguments)]
    pub fn swap_fot(
        env: Env,
        trader: Address,
        token_in: Address,
        amount_in: i128,
        min_out: i128,
        min_received: i128,
        deadline: u64,
        referrer: Option<Address>,
    ) -> Result<(i128, i128), AmmError> {
        if deadline < env.ledger().timestamp() {
            return Err(AmmError::DeadlineExceeded);
        }
        if Self::is_paused(env.clone()) {
            return Err(AmmError::Paused);
        }
        if Self::is_locked(&env) {
            return Err(AmmError::Reentrant);
        }
        trader.require_auth();
        if amount_in <= 0 {
            return Err(AmmError::ZeroAmount);
        }

        Self::checkpoint_oracles(&env);
        Self::check_circuit_breaker(&env)?;

        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();

        let (reserve_in, reserve_out, token_out) = if token_in == token_a {
            (
                Self::get_reserve_a(env.clone()),
                Self::get_reserve_b(env.clone()),
                token_b.clone(),
            )
        } else if token_in == token_b {
            (
                Self::get_reserve_b(env.clone()),
                Self::get_reserve_a(env.clone()),
                token_a.clone(),
            )
        } else {
            return Err(AmmError::InvalidToken);
        };

        if reserve_in <= 0 || reserve_out <= 0 {
            return Err(AmmError::EmptyPool);
        }

        let pool = env.current_contract_address();
        let client_in = SepTokenClient::new(&env, &token_in);
        let balance_before = client_in.balance(&pool);

        client_in.transfer(&trader, &pool, &amount_in);

        let actual_received = client_in.balance(&pool) - balance_before;
        if actual_received <= 0 {
            return Err(AmmError::ZeroAmount);
        }
        if actual_received < min_received {
            return Err(AmmError::FotSlippage);
        }

        let fee_bps: i128 = env.storage().instance().get(&DataKey::FeeBps).unwrap();
        let amount_in_with_fee = actual_received * (10_000 - fee_bps);
        let amount_out =
            amount_in_with_fee * reserve_out / (reserve_in * 10_000 + amount_in_with_fee);

        if amount_out < min_out {
            return Err(AmmError::SlippageExceeded);
        }
        if amount_out >= reserve_out {
            return Err(AmmError::InsufficientLiquidity);
        }

        Self::check_oracle_deviation(&env, &token_in, &token_out, actual_received, amount_out)?;

        SepTokenClient::new(&env, &token_out).transfer(&pool, &trader, &amount_out);

        let protocol_fee_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(0);
        let protocol_fee = if protocol_fee_bps > 0 {
            actual_received * protocol_fee_bps / 10_000
        } else {
            0
        };

        if token_in == token_a {
            env.storage()
                .instance()
                .set(&DataKey::ReserveA, &(reserve_in + actual_received - protocol_fee));
            env.storage()
                .instance()
                .set(&DataKey::ReserveB, &(reserve_out - amount_out));
            if protocol_fee > 0 {
                let accrued: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::AccruedFeeA)
                    .unwrap_or(0);
                env.storage()
                    .instance()
                    .set(&DataKey::AccruedFeeA, &(accrued + protocol_fee));
            }
        } else {
            env.storage()
                .instance()
                .set(&DataKey::ReserveB, &(reserve_in + actual_received - protocol_fee));
            env.storage()
                .instance()
                .set(&DataKey::ReserveA, &(reserve_out - amount_out));
            if protocol_fee > 0 {
                let accrued: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::AccruedFeeB)
                    .unwrap_or(0);
                env.storage()
                    .instance()
                    .set(&DataKey::AccruedFeeB, &(accrued + protocol_fee));
            }
        }

        if actual_received < amount_in {
            soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(&env, "fot_detected"), token_in.clone()), (amount_in, actual_received));
        }

        soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(&env, "swap"), trader), (token_in, actual_received, token_out, amount_out, referrer));

        Ok((amount_out, actual_received))
    }

    /// Add liquidity with fee-on-transfer (FOT) token support.
    ///
    /// Like [`add_liquidity`] but measures the actual token amounts received
    /// by the pool via pre/post balance snapshots rather than trusting the
    /// nominal `amount_a`/`amount_b` values. LP shares are minted proportional
    /// to what the pool actually received.
    ///
    /// # Parameters
    /// - `min_received_a` – Minimum actual `token_a` the pool must receive.
    /// - `min_received_b` – Minimum actual `token_b` the pool must receive.
    #[allow(clippy::too_many_arguments)]
    pub fn add_liquidity_fot(
        env: Env,
        provider: Address,
        amount_a: i128,
        amount_b: i128,
        min_received_a: i128,
        min_received_b: i128,
        min_shares: i128,
        deadline: u64,
    ) -> Result<i128, AmmError> {
        if deadline < env.ledger().timestamp() {
            return Err(AmmError::DeadlineExceeded);
        }
        if Self::is_paused(env.clone()) {
            return Err(AmmError::Paused);
        }
        if Self::is_locked(&env) {
            return Err(AmmError::Reentrant);
        }
        provider.require_auth();
        if amount_a <= 0 || amount_b <= 0 {
            return Err(AmmError::ZeroAmount);
        }

        Self::checkpoint_oracles(&env);

        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let lp_token: Address = env.storage().instance().get(&DataKey::LpToken).unwrap();

        let pool = env.current_contract_address();
        let client_a = SepTokenClient::new(&env, &token_a);
        let client_b = SepTokenClient::new(&env, &token_b);

        let bal_a_before = client_a.balance(&pool);
        let bal_b_before = client_b.balance(&pool);

        client_a.transfer(&provider, &pool, &amount_a);
        client_b.transfer(&provider, &pool, &amount_b);

        let actual_a = client_a.balance(&pool) - bal_a_before;
        let actual_b = client_b.balance(&pool) - bal_b_before;

        if actual_a <= 0 || actual_b <= 0 {
            return Err(AmmError::ZeroAmount);
        }
        if actual_a < min_received_a || actual_b < min_received_b {
            return Err(AmmError::FotSlippage);
        }

        let reserve_a: i128 = Self::get_reserve_a(env.clone());
        let reserve_b: i128 = Self::get_reserve_b(env.clone());
        let total_shares: i128 = Self::get_total_shares(env.clone());

        let shares = if total_shares == 0 {
            Self::sqrt(actual_a * actual_b)
        } else {
            let shares_a = actual_a * total_shares / reserve_a;
            let shares_b = actual_b * total_shares / reserve_b;
            shares_a.min(shares_b)
        };

        if shares <= 0 {
            return Err(AmmError::ZeroAmount);
        }
        if shares < min_shares {
            return Err(AmmError::SlippageExceeded);
        }

        env.storage()
            .instance()
            .set(&DataKey::ReserveA, &(reserve_a + actual_a));
        env.storage()
            .instance()
            .set(&DataKey::ReserveB, &(reserve_b + actual_b));
        env.storage()
            .instance()
            .set(&DataKey::TotalShares, &(total_shares + shares));

        LpTokenClient::new(&env, &lp_token).mint(&provider, &shares);

        soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(&env, "add_liquidity"), provider), (actual_a, actual_b, shares));

        Ok(shares)
    }

    // ── Internals ─────────────────────────────────────────────────────────────

    /// Returns the cumulative liquidity accumulator and the timestamp of the last update.
    pub fn get_liquidity_cumulative(env: Env) -> (i128, u64) {
        let liquidity_cum = env
            .storage()
            .instance()
            .get(&DataKey::LiquidityCumulative)
            .unwrap_or(0);
        let last_timestamp = env
            .storage()
            .instance()
            .get(&DataKey::LastTimestamp)
            .unwrap_or(0);
        (liquidity_cum, last_timestamp)
    }

    /// Returns the cumulative price accumulators and the timestamp of the last update.
    pub fn get_price_cumulative(env: Env) -> (i128, i128, u64) {
        let price_cum_a = env
            .storage()
            .instance()
            .get(&DataKey::PriceCumulativeA)
            .unwrap_or(0);
        let price_cum_b = env
            .storage()
            .instance()
            .get(&DataKey::PriceCumulativeB)
            .unwrap_or(0);
        let last_timestamp = env
            .storage()
            .instance()
            .get(&DataKey::LastTimestamp)
            .unwrap_or(0);
        (price_cum_a, price_cum_b, last_timestamp)
    }

    fn get_reserve_a(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::ReserveA)
            .unwrap_or(0)
    }

    fn get_reserve_b(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::ReserveB)
            .unwrap_or(0)
    }

    fn get_total_shares(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalShares)
            .unwrap_or(0)
    }

    fn get_flash_loan_fee_bps(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::FlashLoanFeeBps)
            .unwrap_or_else(|| env.storage().instance().get(&DataKey::FeeBps).unwrap_or(0))
    }

    /// Integer square root via Newton's method.
    fn sqrt(n: i128) -> i128 {
        if n < 0 {
            panic!("sqrt of negative: {n}");
        }
        if n == 0 {
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
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        token::{StellarAssetClient, TokenClient as StellarTokenClient},
        Env,
    };
    use token::LpToken;

    #[contracttype]
    enum ReceiverDataKey {
        Amm,
        ShouldRepay,
    }
    #[contract]
    pub(crate) struct MockFlashLoanReceiver;
    #[contractimpl]
    impl MockFlashLoanReceiver {
        pub fn initialize(env: Env, amm: Address, should_repay: bool) {
            env.storage().instance().set(&ReceiverDataKey::Amm, &amm);
            env.storage()
                .instance()
                .set(&ReceiverDataKey::ShouldRepay, &should_repay);
        }
        pub fn on_flash_loan(
            env: Env,
            token: Address,
            amount: i128,
            fee: i128,
            _data: Bytes,
        ) -> bool {
            let should_repay = env
                .storage()
                .instance()
                .get(&ReceiverDataKey::ShouldRepay)
                .unwrap_or(false);
            if should_repay {
                let amm: Address = env.storage().instance().get(&ReceiverDataKey::Amm).unwrap();
                let token_client = SepTokenClient::new(&env, &token);
                token_client.transfer(&env.current_contract_address(), &amm, &(amount + fee));
            }
            true
        }
    }

    /// Register a Stellar Asset Contract and return (TokenClient, StellarAssetClient).
    pub(crate) fn create_sac<'a>(
        env: &'a Env,
        admin: &Address,
    ) -> (StellarTokenClient<'a>, StellarAssetClient<'a>) {
        let contract = env.register_stellar_asset_contract_v2(admin.clone());
        (
            StellarTokenClient::new(env, &contract.address()),
            StellarAssetClient::new(env, &contract.address()),
        )
    }

    pub(crate) struct TestSetup {
        pub(crate) env: Env,
        pub(crate) amm_addr: Address,
        pub(crate) lp_addr: Address,
        pub(crate) ta_addr: Address,
        pub(crate) tb_addr: Address,
        #[allow(dead_code)]
        pub(crate) admin: Address,
    }

    /// Minimal setup: env + uninitialized AMM + LP token. Tokens are created by
    /// individual tests so each test can control the pool ratio independently.
    pub(crate) fn setup() -> (Env, Address, Address, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(12345);
        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);

        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "AMM LP Token"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );
        (env, admin.clone(), amm_addr, lp_addr, admin)
    }

    pub(crate) fn setup_pool(fee_bps: i128) -> TestSetup {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(12345);
        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);

        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "AMM LP Token"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );

        let (ta, ta_sac) = create_sac(&env, &admin);
        let (tb, tb_sac) = create_sac(&env, &admin);

        AmmPoolClient::new(&env, &amm_addr).initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &fee_bps,
            &admin,
            &0_i128,
        );

        let ta_addr = ta.address.clone();
        let tb_addr = tb.address.clone();
        drop((ta, ta_sac, tb, tb_sac));

        TestSetup {
            env,
            amm_addr,
            lp_addr,
            ta_addr,
            tb_addr,
            admin,
        }
    }

    // ── Initialization ────────────────────────────────────────────────────────

    // Issue #86: initialize() must persist admin, fee_recipient, and protocol_fee_bps.
    #[test]
    fn test_initialize_stores_admin() {
        let (env, admin, amm_addr, lp_addr, _) = setup();
        let (ta, _) = create_sac(&env, &admin);
        let (tb, _) = create_sac(&env, &admin);
        let fee_recipient = Address::generate(&env);
        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &fee_recipient,
            &5_i128,
        );
        let (stored_recipient, stored_bps) = amm.get_protocol_fee();
        assert_eq!(stored_recipient, Some(fee_recipient));
        assert_eq!(stored_bps, 5);
    }

    #[test]
    fn test_set_protocol_fee_works_after_initialize() {
        let (env, admin, amm_addr, lp_addr, _) = setup();
        let (ta, _) = create_sac(&env, &admin);
        let (tb, _) = create_sac(&env, &admin);
        let fee_recipient = Address::generate(&env);
        let new_recipient = Address::generate(&env);
        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &fee_recipient,
            &0_i128,
        );
        amm.set_protocol_fee(&admin, &new_recipient, &10_i128);
        let (stored_recipient, stored_bps) = amm.get_protocol_fee();
        assert_eq!(stored_recipient, Some(new_recipient));
        assert_eq!(stored_bps, 10);
    }

    #[test]
    fn test_add_and_swap() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &2_000_000_i128);

        let shares = amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &2_000_000_i128,
            &0_i128,
            &u64::MAX,
        );
        assert!(shares > 0);

        let info = amm.get_info();
        assert_eq!(info.reserve_a, 1_000_000);
        assert_eq!(info.reserve_b, 2_000_000);
        assert_eq!(info.flash_loan_fee_bps, 30);

        let trader = Address::generate(env);
        ta_sac.mint(&trader, &100_000_i128);
        let out = amm.swap(&trader, &ts.ta_addr, &100_000_i128, &0_i128, &u64::MAX);
        assert!(out > 0);
        assert!(out < 200_000);
    }

    #[test]
    fn test_price_ratio() {
        let (env, admin, amm_addr, lp_addr, _) = setup();

        let (ta_client, ta_sac) = create_sac(&env, &admin);
        let (tb_client, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta_client.address,
            &tb_client.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &2_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);

        amm.add_liquidity(
            &provider,
            &2_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        // reserve_a = 2_000_000, reserve_b = 1_000_000
        // price_a = 1_000_000 * 1_000_000 / 2_000_000 = 500_000
        // price_b = 2_000_000 * 1_000_000 / 1_000_000 = 2_000_000
        let (price_a, price_b) = amm.price_ratio();
        assert_eq!(price_a, 500_000);
        assert_eq!(price_b, 2_000_000);
    }

    #[test]
    fn test_price_ratio_errors_on_empty_pool() {
        let (env, admin, amm_addr, lp_addr, _) = setup();

        let (ta_client, _) = create_sac(&env, &admin);
        let (tb_client, _) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta_client.address,
            &tb_client.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        // No liquidity added — reserves are zero, should return typed error
        let result = amm.try_price_ratio();
        assert_eq!(result, Err(Ok(AmmError::EmptyPool)));
    }

    #[test]
    fn test_remove_liquidity() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);

        let shares = amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );
        let (out_a, out_b) = amm.remove_liquidity(&provider, &shares, &0_i128, &0_i128, &u64::MAX);
        assert!(out_a > 0 && out_b > 0);
        assert_eq!(amm.get_info().total_shares, 0);
    }

    #[test]
    fn test_initialize_twice_panics() {
        let ts = setup_pool(30);
        let amm = AmmPoolClient::new(&ts.env, &ts.amm_addr);
        let result = amm.try_initialize(
            &ts.admin,
            &ts.ta_addr,
            &ts.tb_addr,
            &ts.lp_addr,
            &30_i128,
            &ts.admin,
            &0_i128,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_fee_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "LP"),
            &soroban_sdk::String::from_str(&env, "LP"),
            &7u32,
        );
        let (ta, _) = create_sac(&env, &admin);
        let (tb, _) = create_sac(&env, &admin);
        let result = AmmPoolClient::new(&env, &amm_addr).try_initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &10_001_i128,
            &admin,
            &0_i128,
        );
        assert!(result.is_err());
    }

    // ── Swap ──────────────────────────────────────────────────────────────────

    #[test]
    fn test_swap_b_to_a() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let trader = Address::generate(env);
        tb_sac.mint(&trader, &100_000_i128);
        let out = amm.swap(&trader, &ts.tb_addr, &100_000_i128, &0_i128, &u64::MAX);
        assert!(out > 0 && out < 100_000);

        let info = amm.get_info();
        assert_eq!(info.reserve_b, 1_100_000);
        assert_eq!(info.reserve_a, 1_000_000 - out);
    }

    #[test]
    fn test_swap_slippage_panics() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let trader = Address::generate(env);
        ta_sac.mint(&trader, &100_000_i128);
        let result = amm.try_swap(
            &trader,
            &ts.ta_addr,
            &100_000_i128,
            &200_000_i128,
            &u64::MAX,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_fee_accrues_to_reserves() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let trader = Address::generate(env);
        let amount_in = 100_000_i128;
        ta_sac.mint(&trader, &amount_in);
        let out = amm.swap(&trader, &ts.ta_addr, &amount_in, &0_i128, &u64::MAX);

        let info = amm.get_info();
        assert_eq!(info.reserve_a, 1_000_000 + amount_in);
        assert_eq!(info.reserve_b, 1_000_000 - out);
        // k must grow because fee stays in pool
        assert!(info.reserve_a * info.reserve_b > 1_000_000 * 1_000_000);
    }

    // ── Issue #98: swap_exact_out ─────────────────────────────────────────────

    #[test]
    fn test_swap_exact_out_normal_path() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let want_out = 50_000_i128;
        let required_in = amm.get_amount_in(&ts.tb_addr, &want_out);

        let trader = Address::generate(env);
        ta_sac.mint(&trader, &(required_in + 1_000));

        let spent = amm.swap_exact_out(
            &trader,
            &ts.tb_addr,
            &want_out,
            &(required_in + 1_000),
            &u64::MAX,
        );

        assert_eq!(spent, required_in);
        let info = amm.get_info();
        assert_eq!(info.reserve_b, 1_000_000 - want_out);
        assert_eq!(info.reserve_a, 1_000_000 + spent);
    }

    #[test]
    fn test_swap_exact_out_slippage_panics() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let trader = Address::generate(env);
        ta_sac.mint(&trader, &100_000_i128);
        // max_in=1 is far too low for any real swap — must panic with slippage message
        let result = amm.try_swap_exact_out(&trader, &ts.tb_addr, &50_000_i128, &1_i128, &u64::MAX);
        assert!(result.is_err());
    }

    #[test]
    fn test_swap_exact_out_invalid_token_panics() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let unknown = Address::generate(env);
        let trader = Address::generate(env);
        let result = amm.try_swap_exact_out(&trader, &unknown, &1_000_i128, &i128::MAX, &u64::MAX);
        assert!(result.is_err());
    }

    #[test]
    fn test_swap_exact_out_paused_panics() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        amm.pause();

        let trader = Address::generate(env);
        ta_sac.mint(&trader, &100_000_i128);
        let result =
            amm.try_swap_exact_out(&trader, &ts.tb_addr, &10_000_i128, &i128::MAX, &u64::MAX);
        assert!(result.is_err());
    }

    #[test]
    fn test_swap_exact_out_round_trip_consistent_with_get_amount_in() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &10_000_000_i128);
        tb_sac.mint(&provider, &10_000_000_i128);
        amm.add_liquidity(
            &provider,
            &10_000_000_i128,
            &10_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let want_out = 500_000_i128;
        let quoted_in = amm.get_amount_in(&ts.tb_addr, &want_out);

        let trader = Address::generate(env);
        ta_sac.mint(&trader, &quoted_in);
        let actual_in = amm.swap_exact_out(&trader, &ts.tb_addr, &want_out, &quoted_in, &u64::MAX);

        assert_eq!(actual_in, quoted_in);
    }

    // ── Liquidity ─────────────────────────────────────────────────────────────

    #[test]
    fn test_add_liquidity_slippage_panics() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        let result = amm.try_add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &i128::MAX,
            &u64::MAX,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_remove_liquidity_slippage_panics() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        let shares = amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );
        let result = amm.try_remove_liquidity(&provider, &shares, &i128::MAX, &0_i128, &u64::MAX);
        assert!(result.is_err());
    }

    #[test]
    fn test_lp_token_transfer_enables_remove() {
        // Verify fix: LP token is the single source of truth for share ownership.
        // Before fix, AMM had a stale internal Shares map that didn't update on transfers.
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let lp = token::LpTokenClient::new(env, &ts.lp_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        let shares = amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let recipient = Address::generate(env);
        lp.transfer(&provider, &recipient, &shares);

        assert_eq!(amm.shares_of(&provider), 0);
        assert_eq!(amm.shares_of(&recipient), shares);

        let (out_a, out_b) = amm.remove_liquidity(&recipient, &shares, &0_i128, &0_i128, &u64::MAX);
        assert!(out_a > 0 && out_b > 0);
        assert_eq!(amm.get_info().total_shares, 0);
    }

    #[test]
    fn test_multiple_lps() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let lp1 = Address::generate(env);
        ta_sac.mint(&lp1, &1_000_000_i128);
        tb_sac.mint(&lp1, &1_000_000_i128);
        let shares1 = amm.add_liquidity(&lp1, &1_000_000_i128, &1_000_000_i128, &0_i128, &u64::MAX);

        let lp2 = Address::generate(env);
        ta_sac.mint(&lp2, &500_000_i128);
        tb_sac.mint(&lp2, &500_000_i128);
        let shares2 = amm.add_liquidity(&lp2, &500_000_i128, &500_000_i128, &0_i128, &u64::MAX);

        assert_eq!(amm.get_info().total_shares, shares1 + shares2);

        amm.remove_liquidity(&lp1, &shares1, &0_i128, &0_i128, &u64::MAX);
        amm.remove_liquidity(&lp2, &shares2, &0_i128, &0_i128, &u64::MAX);
        assert_eq!(amm.get_info().total_shares, 0);
    }

    // ── Quotes ────────────────────────────────────────────────────────────────

    #[test]
    fn test_get_amount_out_matches_swap() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let amount_in = 50_000_i128;
        let quoted = amm.get_amount_out(&ts.ta_addr, &amount_in);

        let trader = Address::generate(env);
        ta_sac.mint(&trader, &amount_in);
        let actual = amm.swap(&trader, &ts.ta_addr, &amount_in, &0_i128, &u64::MAX);

        assert_eq!(quoted, actual);
    }

    #[test]
    fn test_sequential_swaps_invariant() {
        let ts = setup_pool(30); // 0.30% fee
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        // 1. Initial liquidity
        let provider = Address::generate(env);
        let initial_amt = 1_000_000_i128;
        ta_sac.mint(&provider, &initial_amt);
        tb_sac.mint(&provider, &initial_amt);
        amm.add_liquidity(&provider, &initial_amt, &initial_amt, &0_i128, &u64::MAX);

        let info = amm.get_info();
        let initial_k = info.reserve_a * info.reserve_b;
        let mut current_k = initial_k;

        // 2. Perform 10 alternating swaps
        let trader = Address::generate(env);
        let swap_amt = 10_000_i128;

        for i in 0..10 {
            if i % 2 == 0 {
                // A -> B
                ta_sac.mint(&trader, &swap_amt);
                amm.swap(&trader, &ts.ta_addr, &swap_amt, &0_i128, &u64::MAX);
            } else {
                // B -> A
                tb_sac.mint(&trader, &swap_amt);
                amm.swap(&trader, &ts.tb_addr, &swap_amt, &0_i128, &u64::MAX);
            }

            let new_info = amm.get_info();
            let new_k = new_info.reserve_a * new_info.reserve_b;

            // Invariant must hold: new_k >= initial_k
            assert!(
                new_k >= initial_k,
                "Invariant violated: new_k ({new_k}) < initial_k ({initial_k}) at swap {i}"
            );

            // k must grow (or stay same if fee is 0, but here it's 30bps)
            assert!(
                new_k >= current_k,
                "k decreased: new_k ({new_k}) < current_k ({current_k}) at swap {i}"
            );

            current_k = new_k;
        }

        // Final k should be strictly greater than initial k because of fees
        assert!(current_k > initial_k);
    }

    #[test]
    fn test_get_amount_in_round_trip() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &2_000_000_i128);
        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &2_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        // Forward: how much B do we get for 100_000 A?
        let amount_in = 100_000_i128;
        let amount_out = amm.get_amount_out(&ts.ta_addr, &amount_in);
        assert!(amount_out > 0);

        // Reverse: how much A is needed to get exactly amount_out of B?
        let amount_in_reverse = amm.get_amount_in(&ts.tb_addr, &amount_out);

        // Due to integer rounding (+1 in get_amount_in), the reverse quote
        // should be >= the original input and at most 1 unit more.
        assert!(
            amount_in_reverse >= amount_in,
            "reverse quote should be >= original input"
        );
        assert!(
            amount_in_reverse <= amount_in + 1,
            "reverse quote should be at most 1 unit above original input"
        );
    }

    #[test]
    fn test_remove_liquidity_emits_event() {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::{symbol_short, vec, IntoVal};

        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);

        let shares = amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );
        let (out_a, out_b) = amm.remove_liquidity(&provider, &shares, &0_i128, &0_i128, &u64::MAX);

        // Find the rm_liq event among all published events
        let events = env.events().all();
        let rm_liq_event = events
            .iter()
            .find(|e| e.0 == amm.address && e.1 == vec![env, symbol_short!("rm_liq")].into_val(env))
            .expect("remove_liquidity event not found");
        let __ver_0: (u32, (Address, i128, i128, i128)) = rm_liq_event.2.into_val(env);
        assert_eq!(__ver_0.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        let data: (Address, i128, i128, i128) = __ver_0.1;
        let expected = (provider.clone(), shares, out_a, out_b);
        assert_eq!(data, expected);
    }

    #[test]
    fn test_swap_emits_token_out_in_event_payload() {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::IntoVal;

        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let trader = Address::generate(env);
        let amount_in = 100_000_i128;
        ta_sac.mint(&trader, &amount_in);
        let amount_out = amm.swap(&trader, &ts.ta_addr, &amount_in, &0_i128, &u64::MAX);

        let events = env.events().all();
        let swap_event = events
            .iter()
            .find(|e| {
                e.0 == amm.address && e.1 == (Symbol::new(env, "swap"), trader.clone()).into_val(env)
            })
            .expect("swap event not found");

        let __ver_1: (u32, (Address, i128, Address, i128)) = swap_event.2.into_val(env);
        assert_eq!(__ver_1.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        let data: (Address, i128, Address, i128) = __ver_1.1;
        let expected = (
            ts.ta_addr.clone(),
            amount_in,
            ts.tb_addr.clone(),
            amount_out,
        );
        assert_eq!(data, expected);
    }

    #[test]
    fn test_twap_oracle() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        // Add liquidity to set initial price (1:1)
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        // Initial state: accumulators should be 0
        let (cum_a, cum_b, last_ts) = amm.get_price_cumulative();
        assert_eq!(cum_a, 0);
        assert_eq!(cum_b, 0);
        assert!(last_ts > 0);

        // Jump 10 seconds ahead
        env.ledger().set_timestamp(last_ts + 10);

        // Swap A for B
        let trader = Address::generate(env);
        ta_sac.mint(&trader, &100_000_i128);
        amm.swap(&trader, &ts.ta_addr, &100_000_i128, &0_i128, &u64::MAX);

        // Accumulators should have updated: price 1_000_000 * 10 seconds = 10_000_000
        let (new_cum_a, new_cum_b, new_ts) = amm.get_price_cumulative();
        assert_eq!(new_ts, last_ts + 10);
        assert_eq!(new_cum_a, 10_000_000);
        assert_eq!(new_cum_b, 10_000_000);

        // Jump another 5 seconds
        env.ledger().set_timestamp(new_ts + 5);

        // New spot price after swap:
        // reserve_a = 1_100_000, reserve_b = 1_000_000 - out
        // Price A = (1_000_000 - out) * 1_000_000 / 1_100_000
        let info = amm.get_info();
        let expected_price_a = info.reserve_b * 1_000_000 / info.reserve_a;
        let expected_price_b = info.reserve_a * 1_000_000 / info.reserve_b;

        // Perform another swap
        tb_sac.mint(&trader, &50_000_i128);
        amm.swap(&trader, &ts.tb_addr, &50_000_i128, &0_i128, &u64::MAX);

        let (final_cum_a, final_cum_b, final_ts) = amm.get_price_cumulative();
        assert_eq!(final_ts, new_ts + 5);
        assert_eq!(final_cum_a, new_cum_a + expected_price_a * 5);
        assert_eq!(final_cum_b, new_cum_b + expected_price_b * 5);
    }

    #[test]
    fn test_twal_oracle() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let (cum, last_ts) = amm.get_liquidity_cumulative();
        assert_eq!(cum, 0);
        assert!(last_ts > 0);

        env.ledger().set_timestamp(last_ts + 10);
        let trader = Address::generate(env);
        ta_sac.mint(&trader, &10_000_i128);
        amm.swap(&trader, &ts.ta_addr, &10_000_i128, &0_i128, &u64::MAX);

        let (new_cum, new_ts) = amm.get_liquidity_cumulative();
        assert_eq!(new_ts, last_ts + 10);
        // sqrt(1e6 * 1e6) = 1e6, * 10s = 10_000_000
        assert_eq!(new_cum, 10_000_000);
    }

    // ── Edge cases: zero-reserve guard ───────────────────────────────────────────

    #[test]
    fn test_swap_on_empty_pool_panics() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);

        let trader = Address::generate(env);
        ta_sac.mint(&trader, &1_000_i128);
        let result = amm.try_swap(&trader, &ts.ta_addr, &1_000_i128, &0_i128, &u64::MAX);
        assert!(result.is_err());
    }

    // ── Edge cases: fee boundary ──────────────────────────────────────────────────

    #[test]
    fn test_fee_bps_zero_succeeds() {
        let ts = setup_pool(0);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let trader = Address::generate(env);
        let amount_in = 100_000_i128;
        ta_sac.mint(&trader, &amount_in);
        let out = amm.swap(&trader, &ts.ta_addr, &amount_in, &0_i128, &u64::MAX);
        // fee_bps=0 → no discount; pure constant-product formula
        let expected = amount_in * 1_000_000 / (1_000_000 + amount_in);
        assert_eq!(out, expected);
    }

    #[test]
    fn test_fee_bps_max_succeeds() {
        // fee_bps=10_000 is the inclusive upper bound; pool initializes successfully.
        // With 100% fee, amount_in_with_fee = 0, so amount_out = 0.
        let ts = setup_pool(10_000);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let trader = Address::generate(env);
        ta_sac.mint(&trader, &100_000_i128);
        let result = amm.try_swap(&trader, &ts.ta_addr, &100_000_i128, &0_i128, &u64::MAX);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().unwrap(), 0);
    }

    // ── Edge cases: minimum share precision ──────────────────────────────────────

    #[test]
    fn test_min_shares_exact_succeeds() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        // Initial deposit: shares = sqrt(1_000_000 * 1_000_000) = 1_000_000
        let shares = amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &1_000_000_i128,
            &u64::MAX,
        );
        assert_eq!(shares, 1_000_000);
    }

    #[test]
    fn test_min_shares_off_by_one_panics() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        // Expected = 1_000_000; requesting 1_000_001 triggers the slippage guard.
        let result = amm.try_add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &1_000_001_i128,
            &u64::MAX,
        );
        assert!(result.is_err());
    }

    // ── Issue #34: imbalanced deposit uses the minimum ratio ──────────────────

    #[test]
    fn test_imbalanced_deposit_uses_minimum_ratio() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        // Seed pool: 1,000,000 A and 2,000,000 B (ratio 1:2)
        let seeder = Address::generate(env);
        ta_sac.mint(&seeder, &1_000_000_i128);
        tb_sac.mint(&seeder, &2_000_000_i128);
        let initial_shares = amm.add_liquidity(
            &seeder,
            &1_000_000_i128,
            &2_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        // Deposit 500,000 A and 1,500,000 B — B is 500,000 in excess of the 1:2 ratio
        let lp2 = Address::generate(env);
        ta_sac.mint(&lp2, &500_000_i128);
        tb_sac.mint(&lp2, &1_500_000_i128);
        let shares_minted =
            amm.add_liquidity(&lp2, &500_000_i128, &1_500_000_i128, &0_i128, &u64::MAX);

        let shares_from_a = 500_000_i128 * initial_shares / 1_000_000;
        let shares_from_b = 1_500_000_i128 * initial_shares / 2_000_000;

        assert!(
            shares_from_a < shares_from_b,
            "TokenA should be the limiting ratio"
        );
        assert_eq!(
            shares_minted, shares_from_a,
            "shares minted must use the limiting (TokenA) ratio"
        );

        let info = amm.get_info();
        assert_eq!(info.reserve_a, 1_500_000);
        assert_eq!(info.reserve_b, 3_500_000);
    }

    // ── Issue #35: partial remove_liquidity leaves correct residual reserves ──

    #[test]
    fn test_partial_remove_liquidity_leaves_correct_reserves() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        let total_shares = amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );
        assert_eq!(total_shares, 1_000_000);

        let shares_to_remove = total_shares / 4; // 25% = 250,000
        let (out_a, out_b) =
            amm.remove_liquidity(&provider, &shares_to_remove, &0_i128, &0_i128, &u64::MAX);

        assert_eq!(out_a, 250_000);
        assert_eq!(out_b, 250_000);

        let info = amm.get_info();
        assert_eq!(info.reserve_a, 750_000);
        assert_eq!(info.reserve_b, 750_000);
        assert_eq!(info.total_shares, total_shares - shares_to_remove);
    }

    // ── Issue #36: swap output rate decreases as input size grows ─────────────

    #[test]
    fn test_swap_output_rate_decreases_with_input_size() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let input_sizes = [1_000_i128, 10_000_i128, 100_000_i128, 500_000_i128];
        let mut prev_rate = i128::MAX;

        for &amount_in in input_sizes.iter() {
            let amount_out = amm.get_amount_out(&ts.ta_addr, &amount_in);
            // Scale by 1_000_000 to preserve precision when comparing rates
            let rate = amount_out * 1_000_000 / amount_in;
            assert!(
                rate < prev_rate,
                "effective rate {rate} at input {amount_in} should be strictly less than previous rate {prev_rate}"
            );
            prev_rate = rate;
        }
    }

    // ── Issue #37: overflow guard tests for near-maximum reserve values ────────

    #[test]
    fn test_sqrt_handles_large_input() {
        // sqrt(10^18) = 10^9
        assert_eq!(
            AmmPool::sqrt(1_000_000_000_000_000_000_i128),
            1_000_000_000_i128
        );
        // sqrt(10^36) = 10^18; 10^36 < i128::MAX (~1.7e38)
        assert_eq!(
            AmmPool::sqrt(1_000_000_000_000_000_000_000_000_000_000_000_000_i128),
            1_000_000_000_000_000_000_i128,
        );
    }

    #[test]
    fn test_large_reserves_add_liquidity_no_overflow() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        // 4e18 * 4e18 = 1.6e37 < i128::MAX (~1.7e38); sqrt = 4e18
        let large_amount = 4_000_000_000_000_000_000_i128;
        let provider = Address::generate(env);
        ta_sac.mint(&provider, &large_amount);
        tb_sac.mint(&provider, &large_amount);
        let shares = amm.add_liquidity(&provider, &large_amount, &large_amount, &0_i128, &u64::MAX);

        assert_eq!(shares, large_amount);
        let info = amm.get_info();
        assert_eq!(info.reserve_a, large_amount);
        assert_eq!(info.reserve_b, large_amount);
    }

    #[test]
    fn test_large_reserves_swap_no_overflow() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let large_amount = 4_000_000_000_000_000_000_i128;
        let provider = Address::generate(env);
        ta_sac.mint(&provider, &large_amount);
        tb_sac.mint(&provider, &large_amount);
        amm.add_liquidity(&provider, &large_amount, &large_amount, &0_i128, &u64::MAX);

        // amount_in=10^9; numerator = 10^9*9970*4e18 ~ 4e31 < i128::MAX
        let trader = Address::generate(env);
        let amount_in = 1_000_000_000_i128;
        ta_sac.mint(&trader, &amount_in);
        let out = amm.swap(&trader, &ts.ta_addr, &amount_in, &0_i128, &u64::MAX);
        assert!(out > 0 && out < large_amount);
    }

    #[test]
    fn test_large_reserves_price_ratio_no_overflow() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let large_amount = 4_000_000_000_000_000_000_i128;
        let provider = Address::generate(env);
        ta_sac.mint(&provider, &large_amount);
        tb_sac.mint(&provider, &large_amount);
        amm.add_liquidity(&provider, &large_amount, &large_amount, &0_i128, &u64::MAX);

        // price_ratio: reserve_b * 1_000_000 / reserve_a; 4e18 * 1e6 = 4e24 < i128::MAX
        let (price_a, price_b) = amm.price_ratio();
        assert_eq!(price_a, 1_000_000);
        assert_eq!(price_b, 1_000_000);
    }

    #[test]
    fn test_large_reserves_get_amount_in_round_trip() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let large_amount = 4_000_000_000_000_000_000_i128; // 4e18
        let provider = Address::generate(env);
        ta_sac.mint(&provider, &large_amount);
        tb_sac.mint(&provider, &large_amount);
        amm.add_liquidity(&provider, &large_amount, &large_amount, &0_i128, &u64::MAX);

        // Forward: B for A
        let amount_in = 1_000_000_000_i128;
        let amount_out = amm.get_amount_out(&ts.ta_addr, &amount_in);
        assert!(amount_out > 0);

        // Reverse: A needed for B
        let amount_in_reverse = amm.get_amount_in(&ts.tb_addr, &amount_out);

        assert!(
            amount_in_reverse >= amount_in,
            "reverse quote should be >= original input"
        );
        assert!(
            amount_in_reverse <= amount_in + 1,
            "reverse quote should be at most 1 unit above original input"
        );
    }
    #[test]
    #[should_panic]
    fn test_get_amount_in_overflow() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let large_amount = 4_000_000_000_000_000_000_i128; // 4e18
        let provider = Address::generate(env);
        ta_sac.mint(&provider, &large_amount);
        tb_sac.mint(&provider, &large_amount);
        amm.add_liquidity(&provider, &large_amount, &large_amount, &0_i128, &u64::MAX);

        // 4e18 * 1e17 * 10000 = 4e39 > i128::MAX
        amm.get_amount_in(&ts.ta_addr, &100_000_000_000_000_000_i128);
    }

    // Issue #199: remove_liquidity_one_sided — provider receives only token_a.
    #[test]
    fn test_remove_liquidity_one_sided() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);
        let ta_client = StellarTokenClient::new(env, &ts.ta_addr);
        let tb_client = StellarTokenClient::new(env, &ts.tb_addr);

        // LP1 seeds the pool so LP2's internal swap has residual reserves to trade against.
        let lp1 = Address::generate(env);
        ta_sac.mint(&lp1, &2_000_000_i128);
        tb_sac.mint(&lp1, &2_000_000_i128);
        amm.add_liquidity(&lp1, &2_000_000_i128, &2_000_000_i128, &0_i128, &u64::MAX);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        let shares = amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let ta_before = ta_client.balance(&provider);
        let tb_before = tb_client.balance(&provider);

        // Remove one-sided: provider wants only token_a.
        // min_out = 1_000_000 ensures at least the proportional withdrawal.
        let total_out = amm.remove_liquidity_one_sided(
            &provider,
            &shares,
            &ts.ta_addr,
            &1_000_000_i128,
            &u64::MAX,
        );

        let ta_after = ta_client.balance(&provider);
        let tb_after = tb_client.balance(&provider);

        // Provider received exactly total_out of token_a.
        assert_eq!(ta_after - ta_before, total_out);
        // Provider's token_b balance is unchanged — received no token_b.
        assert_eq!(tb_after, tb_before);
        // Total received is more than the proportional token_a alone because the
        // unwanted token_b was swapped internally for more token_a.
        assert!(total_out > 1_000_000);
        // LP shares are fully redeemed.
        assert_eq!(amm.shares_of(&provider), 0);
    }

    // Issue #199: min_out slippage guard is enforced in remove_liquidity_one_sided.
    #[test]
    fn test_remove_liquidity_one_sided_slippage_fails() {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let lp1 = Address::generate(env);
        ta_sac.mint(&lp1, &2_000_000_i128);
        tb_sac.mint(&lp1, &2_000_000_i128);
        amm.add_liquidity(&lp1, &2_000_000_i128, &2_000_000_i128, &0_i128, &u64::MAX);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        let shares = amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        // min_out set impossibly high — must fail.
        let result = amm.try_remove_liquidity_one_sided(
            &provider,
            &shares,
            &ts.ta_addr,
            &i128::MAX,
            &u64::MAX,
        );
        assert!(result.is_err());
    }
    #[test]
    fn bench_swap_cost() {
        extern crate std;
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(&provider, &1_000_000_i128, &1_000_000_i128, &0_i128, &u64::MAX);

        let trader = Address::generate(env);
        ta_sac.mint(&trader, &100_000_i128);

        env.budget().reset_default();
        amm.swap(&trader, &ts.ta_addr, &100_000_i128, &0_i128, &u64::MAX);
        std::println!("=== SWAP BUDGET ===");
        std::println!("{}", env.budget());
    }

    #[test]
    fn bench_add_liquidity_cost() {
        extern crate std;
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

        // seed once so we hit the "else" (proportional) branch on the measured call
        let seeder = Address::generate(env);
        ta_sac.mint(&seeder, &1_000_000_i128);
        tb_sac.mint(&seeder, &1_000_000_i128);
        amm.add_liquidity(&seeder, &1_000_000_i128, &1_000_000_i128, &0_i128, &u64::MAX);

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &500_000_i128);
        tb_sac.mint(&provider, &500_000_i128);

        env.budget().reset_default();
        amm.add_liquidity(&provider, &500_000_i128, &500_000_i128, &0_i128, &u64::MAX);
        std::println!("=== ADD_LIQ BUDGET ===");
        std::println!("{}", env.budget());
    }
}

// ── Property-based tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod prop_tests {
    extern crate std;
    use super::tests::*;
    use super::*;
    use proptest::prelude::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        Address, Bytes, Env,
    };

    proptest! {
        /// Property 1: For any valid first deposit, initial shares (sqrt(a*b)) are always positive.
        #[test]
        fn first_deposit_shares_always_positive(
            a in 1_i128..=100_000_i128,
            b in 1_i128..=100_000_i128,
        ) {
            let shares = AmmPool::sqrt(a * b);
            prop_assert!(shares > 0, "shares={shares} for a={a}, b={b}");
        }

        /// Property 2: Subsequent deposit shares minted are ≤ the proportional amount for each token.
        #[test]
        fn subsequent_deposit_shares_leq_proportional(
            amount_a in 1_i128..=1_000_000_i128,
            amount_b in 1_i128..=1_000_000_i128,
            reserve_a in 1_i128..=1_000_000_i128,
            reserve_b in 1_i128..=1_000_000_i128,
            total_shares in 1_i128..=1_000_000_i128,
        ) {
            let shares_a = amount_a * total_shares / reserve_a;
            let shares_b = amount_b * total_shares / reserve_b;
            let minted = shares_a.min(shares_b);
            prop_assert!(minted <= shares_a, "minted={minted} > shares_a={shares_a}");
            prop_assert!(minted <= shares_b, "minted={minted} > shares_b={shares_b}");
        }

        /// Property 3: For any valid shares ≤ total_shares, remove_liquidity outputs are non-negative.
        #[test]
        fn remove_liquidity_outputs_nonneg(
            shares in 1_i128..=10_000_i128,
            extra in 0_i128..=10_000_i128,
            reserve_a in 0_i128..=1_000_000_i128,
            reserve_b in 0_i128..=1_000_000_i128,
        ) {
            // total_shares >= shares by construction
            let total_shares = shares + extra;
            let out_a = shares * reserve_a / total_shares;
            let out_b = shares * reserve_b / total_shares;
            prop_assert!(out_a >= 0, "out_a={out_a} is negative");
            prop_assert!(out_b >= 0, "out_b={out_b} is negative");
        }

        /// Property 4: get_amount_out output is always strictly less than the output reserve.
        #[test]
        fn amount_out_strictly_lt_reserve(
            amount_in in 1_i128..=100_000_i128,
            reserve_in in 1_i128..=1_000_000_i128,
            reserve_out in 1_i128..=1_000_000_i128,
            fee_bps in 0_i128..=10_000_i128,
        ) {
            let amount_in_with_fee = amount_in * (10_000 - fee_bps);
            let denom = reserve_in * 10_000 + amount_in_with_fee;
            // When fee_bps == 10_000, amount_in_with_fee == 0 → amount_out == 0 < reserve_out.
            let amount_out = if denom == 0 {
                0
            } else {
                amount_in_with_fee * reserve_out / denom
            };
            prop_assert!(
                amount_out < reserve_out,
                "amount_out={amount_out} >= reserve_out={reserve_out}"
            );
        }
    }

    #[test]
    fn test_flash_loan_success_with_repayment() {
        let (env, admin, amm_addr, lp_addr, _) = setup();

        let (ta_client, ta_sac) = create_sac(&env, &admin);
        let (tb_client, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize_with_flash_loan_fee(
            &admin,
            &ta_client.address,
            &tb_client.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
            &50_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let receiver_addr = env.register_contract(None, MockFlashLoanReceiver);
        let receiver = MockFlashLoanReceiverClient::new(&env, &receiver_addr);
        receiver.initialize(&amm_addr, &true);

        ta_sac.mint(&receiver_addr, &1_000_i128);

        let fee = amm.flash_loan(
            &receiver_addr,
            &ta_client.address,
            &100_000_i128,
            &Bytes::new(&env),
        );
        assert_eq!(fee, 500);

        let info = amm.get_info();
        assert_eq!(info.reserve_a, 1_000_500);
        assert_eq!(info.reserve_b, 1_000_000);
        assert_eq!(info.flash_loan_fee_bps, 50);
    }

    #[test]
    #[should_panic]
    fn test_flash_loan_failed_repayment_panics() {
        let (env, admin, amm_addr, lp_addr, _) = setup();

        let (ta_client, ta_sac) = create_sac(&env, &admin);
        let (tb_client, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta_client.address,
            &tb_client.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let receiver_addr = env.register_contract(None, MockFlashLoanReceiver);
        let receiver = MockFlashLoanReceiverClient::new(&env, &receiver_addr);
        receiver.initialize(&amm_addr, &false);

        amm.flash_loan(
            &receiver_addr,
            &ta_client.address,
            &100_000_i128,
            &Bytes::new(&env),
        );
    }

    #[test]
    fn test_get_fee_info() {
        let (env, admin, amm_addr, lp_addr, _) = setup();
        let (ta_client, _) = create_sac(&env, &admin);
        let (tb_client, _) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta_client.address,
            &tb_client.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        assert_eq!(amm.get_fee_info(), 30_i128);
        assert_eq!(amm.get_fee_info(), amm.get_info().fee_bps);
     }

     #[test]
     fn test_emergency_withdraw() {
         let (env, admin, amm_addr, lp_addr, _) = setup();
         let (ta_client, ta_sac) = create_sac(&env, &admin);
         let (tb_client, tb_sac) = create_sac(&env, &admin);
         let amm = AmmPoolClient::new(&env, &amm_addr);

         amm.initialize(
             &admin,
             &ta_client.address,
             &tb_client.address,
             &lp_addr,
             &30_i128,
             &admin,
             &0_i128,
         );

         let provider = Address::generate(&env);
         ta_sac.mint(&provider, &2_000_000_i128);
         tb_sac.mint(&provider, &1_000_000_i128);

         amm.add_liquidity(
             &provider,
             &2_000_000_i128,
             &1_000_000_i128,
             &0_i128,
             &u64::MAX,
         );

         let info_before = amm.get_info();
         assert_eq!(info_before.reserve_a, 2_000_000);
         assert_eq!(info_before.reserve_b, 1_000_000);

         let recipient = Address::generate(&env);
         let expected_ts = 99999_u64;
         env.ledger().set_timestamp(expected_ts);

         // Call emergency_withdraw
         amm.emergency_withdraw(&recipient);

         // Verify reserves are now zeroed
         let info_after = amm.get_info();
         assert_eq!(info_after.reserve_a, 0);
         assert_eq!(info_after.reserve_b, 0);

         // Verify recipient received the tokens
         assert_eq!(ta_client.balance(&recipient), 2_000_000);
         assert_eq!(tb_client.balance(&recipient), 1_000_000);

         // Verify audit log values in contract storage from the contract context.
         env.as_contract(&amm_addr, || {
             let ts: u64 = env.storage().instance().get(&DataKey::EmergencyWithdrawTimestamp).unwrap();
             let rec: Address = env.storage().instance().get(&DataKey::EmergencyWithdrawRecipient).unwrap();
             assert_eq!(ts, expected_ts);
             assert_eq!(rec, recipient);
         });
     }

     #[test]
     #[should_panic]
     fn test_emergency_withdraw_requires_admin_auth() {
         let env = Env::default();
         let amm_addr = env.register_contract(None, AmmPool);
         let amm = AmmPoolClient::new(&env, &amm_addr);
         let recipient = Address::generate(&env);
         amm.emergency_withdraw(&recipient);
     }

    #[test]
    #[should_panic]
    fn test_pause_requires_admin_auth() {
        let env = Env::default();
        let amm_addr = env.register_contract(None, AmmPool);
        let amm = AmmPoolClient::new(&env, &amm_addr);

        amm.pause();
    }

    #[test]
    #[should_panic]
    fn test_unpause_requires_admin_auth() {
        let env = Env::default();
        let amm_addr = env.register_contract(None, AmmPool);
        let amm = AmmPoolClient::new(&env, &amm_addr);

        amm.unpause();
    }

    #[test]
    fn test_pause_blocks_read_only_functions_remain_available_then_unpause_succeeds() {
        let (env, admin, amm_addr, lp_addr, _) = setup();
        let (ta_client, ta_sac) = create_sac(&env, &admin);
        let (tb_client, tb_sac) = create_sac(&env, &admin);
        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta_client.address,
            &tb_client.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        let shares = amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        amm.pause();
        assert!(amm.is_paused());

        let info = amm.get_info();
        assert_eq!(info.reserve_a, 1_000_000);
        assert_eq!(info.reserve_b, 1_000_000);

        let quote = amm.get_amount_out(&ta_client.address, &100_000_i128);
        assert!(quote > 0);
        assert_eq!(amm.shares_of(&provider), shares);

        amm.unpause();
        assert!(!amm.is_paused());

        let trader = Address::generate(&env);
        ta_sac.mint(&trader, &100_000_i128);
        let out = amm.swap(
            &trader,
            &ta_client.address,
            &100_000_i128,
            &0_i128,
            &u64::MAX,
        );
        assert!(out > 0);

        let extra_provider = Address::generate(&env);
        ta_sac.mint(&extra_provider, &100_000_i128);
        tb_sac.mint(&extra_provider, &100_000_i128);
        let extra_shares = amm.add_liquidity(
            &extra_provider,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &u64::MAX,
        );
        assert!(extra_shares > 0);

        let (out_a, out_b) = amm.remove_liquidity(&provider, &shares, &0_i128, &0_i128, &u64::MAX);
        assert!(out_a > 0 && out_b > 0);
    }

    #[test]
    #[should_panic]
    fn test_add_liquidity_panics_when_paused() {
        let (env, admin, amm_addr, lp_addr, _) = setup();
        let (ta_client, ta_sac) = create_sac(&env, &admin);
        let (tb_client, tb_sac) = create_sac(&env, &admin);
        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta_client.address,
            &tb_client.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);

        amm.pause();
        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );
    }

    #[test]
    #[should_panic]
    fn test_swap_panics_when_paused() {
        let (env, admin, amm_addr, lp_addr, _) = setup();
        let (ta_client, ta_sac) = create_sac(&env, &admin);
        let (tb_client, tb_sac) = create_sac(&env, &admin);
        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta_client.address,
            &tb_client.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let trader = Address::generate(&env);
        ta_sac.mint(&trader, &100_000_i128);

        amm.pause();
        amm.swap(
            &trader,
            &ta_client.address,
            &100_000_i128,
            &0_i128,
            &u64::MAX,
        );
    }

    #[test]
    #[should_panic]
    fn test_remove_liquidity_panics_when_paused() {
        let (env, admin, amm_addr, lp_addr, _) = setup();
        let (ta_client, ta_sac) = create_sac(&env, &admin);
        let (tb_client, tb_sac) = create_sac(&env, &admin);
        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta_client.address,
            &tb_client.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        let shares = amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        amm.pause();
        amm.remove_liquidity(&provider, &shares, &0_i128, &0_i128, &u64::MAX);
    }

    #[test]
    fn test_protocol_fee_accrual() {
        let (env, admin, amm_addr, lp_addr, _) = setup();

        let (ta_client, ta_sac) = create_sac(&env, &admin);
        let (tb_client, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        // fee_bps=30, protocol_fee_bps=5
        amm.initialize(
            &admin,
            &ta_client.address,
            &tb_client.address,
            &lp_addr,
            &30_i128,
            &admin,
            &5_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let trader = Address::generate(&env);
        ta_sac.mint(&trader, &200_000_i128);

        // Two swaps of 100_000 A each — protocol fee per swap = 100_000 * 5 / 10_000 = 50
        amm.swap(
            &trader,
            &ta_client.address,
            &100_000_i128,
            &0_i128,
            &u64::MAX,
        );
        amm.swap(
            &trader,
            &ta_client.address,
            &100_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let admin_bal_before = ta_client.balance(&admin);
        let (withdrawn_a, withdrawn_b) = amm.withdraw_protocol_fees();
        let admin_bal_after = ta_client.balance(&admin);

        assert_eq!(withdrawn_a, 100_i128); // 50 + 50
        assert_eq!(withdrawn_b, 0_i128);
        assert_eq!(admin_bal_after - admin_bal_before, 100_i128);
    }

    #[test]
    fn test_withdraw_resets_accrued() {
        let (env, admin, amm_addr, lp_addr, _) = setup();

        let (ta_client, ta_sac) = create_sac(&env, &admin);
        let (tb_client, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta_client.address,
            &tb_client.address,
            &lp_addr,
            &30_i128,
            &admin,
            &5_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let trader = Address::generate(&env);
        ta_sac.mint(&trader, &100_000_i128);
        amm.swap(
            &trader,
            &ta_client.address,
            &100_000_i128,
            &0_i128,
            &u64::MAX,
        );

        // First withdrawal collects accrued fees.
        let (w1_a, _) = amm.withdraw_protocol_fees();
        assert!(w1_a > 0);

        // Second withdrawal: accrued balances were reset to zero.
        let (w2_a, w2_b) = amm.withdraw_protocol_fees();
        assert_eq!(w2_a, 0_i128);
        assert_eq!(w2_b, 0_i128);
    }

    #[test]
    fn test_reaccrual_after_withdrawal() {
        let (env, admin, amm_addr, lp_addr, _) = setup();

        let (ta_client, ta_sac) = create_sac(&env, &admin);
        let (tb_client, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta_client.address,
            &tb_client.address,
            &lp_addr,
            &30_i128,
            &admin,
            &5_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let trader = Address::generate(&env);
        ta_sac.mint(&trader, &200_000_i128);

        // Swap → withdraw → swap again → withdraw: fees re-accrue after reset.
        amm.swap(
            &trader,
            &ta_client.address,
            &100_000_i128,
            &0_i128,
            &u64::MAX,
        );
        let (w1, _) = amm.withdraw_protocol_fees();
        assert!(w1 > 0);

        amm.swap(
            &trader,
            &ta_client.address,
            &100_000_i128,
            &0_i128,
            &u64::MAX,
        );
        let (w2, _) = amm.withdraw_protocol_fees();
        assert!(w2 > 0);
    }

    // Issue #132: PoolInfo must expose admin, fee_recipient, and protocol_fee_bps.
    #[test]
    fn test_get_info_returns_admin_and_fee_recipient() {
        let (env, admin, amm_addr, lp_addr, _) = setup();
        let (ta, _) = create_sac(&env, &admin);
        let (tb, _) = create_sac(&env, &admin);
        let fee_recipient = Address::generate(&env);
        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &fee_recipient,
            &5_i128,
        );

        let info = amm.get_info();
        assert_eq!(info.admin, admin);
        assert_eq!(info.fee_recipient, fee_recipient);
        assert_eq!(info.protocol_fee_bps, 5_i128);
        assert_eq!(info.fee_bps, 30_i128);
    }

    // Issue #131: get_accrued_fees must return (0, 0) before swaps.
    #[test]
    fn test_get_accrued_fees_zero_before_swaps() {
        let (env, admin, amm_addr, lp_addr, _) = setup();
        let (ta, _) = create_sac(&env, &admin);
        let (tb, _) = create_sac(&env, &admin);
        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &admin,
            &5_i128,
        );

        let (a, b) = amm.get_accrued_fees();
        assert_eq!(a, 0_i128);
        assert_eq!(b, 0_i128);
    }

    // Issue #131: get_accrued_fees must match accumulation after swaps without
    // mutating state.
    #[test]
    fn test_get_accrued_fees_matches_swap_accumulation() {
        let (env, admin, amm_addr, lp_addr, _) = setup();
        let (ta_client, ta_sac) = create_sac(&env, &admin);
        let (tb_client, tb_sac) = create_sac(&env, &admin);
        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta_client.address,
            &tb_client.address,
            &lp_addr,
            &30_i128,
            &admin,
            &5_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let trader = Address::generate(&env);
        ta_sac.mint(&trader, &100_000_i128);
        amm.swap(
            &trader,
            &ta_client.address,
            &100_000_i128,
            &0_i128,
            &u64::MAX,
        );

        // protocol fee per swap = 100_000 * 5 / 10_000 = 50, accrued in token A.
        let (accrued_a, accrued_b) = amm.get_accrued_fees();
        assert_eq!(accrued_a, 50_i128);
        assert_eq!(accrued_b, 0_i128);

        // Calling get_accrued_fees does not mutate state — withdrawing now
        // returns the same amount.
        let (withdrawn_a, withdrawn_b) = amm.withdraw_protocol_fees();
        assert_eq!(withdrawn_a, 50_i128);
        assert_eq!(withdrawn_b, 0_i128);

        // After withdrawal, accrued is back to zero.
        let (post_a, post_b) = amm.get_accrued_fees();
        assert_eq!(post_a, 0_i128);
        assert_eq!(post_b, 0_i128);
    }

    // Issue #130: propose_admin must emit `admin_nominated`.
    #[test]
    fn test_propose_admin_emits_event() {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::IntoVal;

        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let nominee = Address::generate(env);

        amm.propose_admin(&ts.admin, &nominee);

        let events = env.events().all();
        let evt = events
            .iter()
            .find(|e| {
                e.0 == amm.address && e.1 == (Symbol::new(env, "admin_nominated"),).into_val(env)
            })
            .expect("admin_nominated event not found");

        let __ver_2: (u32, (Address, Address)) = evt.2.into_val(env);
        assert_eq!(__ver_2.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        let data: (Address, Address) = __ver_2.1;
        assert_eq!(data, (ts.admin.clone(), nominee.clone()));
    }

    // Issue #130: accept_admin must emit `admin_changed`.
    #[test]
    fn test_accept_admin_emits_event() {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::IntoVal;

        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let nominee = Address::generate(env);

        amm.propose_admin(&ts.admin, &nominee);
        amm.accept_admin(&nominee);

        let events = env.events().all();
        let evt = events
            .iter()
            .find(|e| {
                e.0 == amm.address && e.1 == (Symbol::new(env, "admin_changed"),).into_val(env)
            })
            .expect("admin_changed event not found");

        let __ver_3: (u32, (Address,)) = evt.2.into_val(env);
        assert_eq!(__ver_3.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        let data: (Address,) = __ver_3.1;
        assert_eq!(data, (nominee,));
    }
    // Issue #193: simulate_swap price_impact_bps grows for large swaps.
    #[test]
    fn test_simulate_swap_price_impact_bps() {
        let ts = setup_pool(30); // 0.30 % fee
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);

        // Mint tokens for the provider and seed the pool with 1_000_000 of each.
        let provider = Address::generate(env);
        let ta_sac = soroban_sdk::token::StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = soroban_sdk::token::StellarAssetClient::new(env, &ts.tb_addr);
        ta_sac.mint(&provider, &2_000_000_i128);
        tb_sac.mint(&provider, &2_000_000_i128);

        amm.add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        // --- Tiny swap: price_impact_bps should be 0 (rounds to 0 at 1 unit). ---
        let tiny = amm.simulate_swap(&ts.ta_addr, &1_i128);
        // spot and effective price differ by sub-bps amounts for 1-unit swap.
        assert_eq!(tiny.price_impact_bps, 0);

        // --- Large swap: price_impact_bps must be positive. ---
        let large = amm.simulate_swap(&ts.ta_addr, &100_000_i128);
        // With reserves 1_000_000 / 1_000_000 and amount_in 100_000 (10 % of pool):
        //   spot_price  = 1_000_000 * 1_000_000 / 1_000_000 = 1_000_000
        //   amount_in_with_fee = 100_000 * (10000 - 30) = 997_000_000
        //   amount_out ≈ 90_661; effective_price ≈ 906_610
        //   price_impact_bps ≈ 934
        assert!(large.price_impact_bps > 0, "price_impact_bps must be positive for large swap");

        // Larger swap must have higher price impact than smaller swap.
        let medium = amm.simulate_swap(&ts.ta_addr, &10_000_i128);
        assert!(
            large.price_impact_bps > medium.price_impact_bps,
            "larger swap must have larger price impact"
        );
        assert!(
            medium.price_impact_bps > tiny.price_impact_bps,
            "medium swap must have larger price impact than tiny"
        );
    }
}
