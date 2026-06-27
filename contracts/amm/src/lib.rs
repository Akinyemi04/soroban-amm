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
    contract, contractclient, contracterror, contractimpl, contracttype, symbol_short, Address,
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
    AlreadyInitialized = 1,
    InvalidFeeBps = 2,
    InsufficientShares = 3,
    DeadlineExceeded = 4,
    SlippageExceeded = 5,
    Paused = 6,
    Unauthorized = 7,
    ZeroAmount = 8,
    InvalidToken = 9,
    EmptyPool = 10,
    InsufficientLiquidity = 11,
    NoPendingAdmin = 12,
    WrongAdmin = 13,
    /// A reentrant call was detected while a flash loan or state-mutating
    /// operation was already in progress. The receiver contract must not
    /// call back into this pool during an `on_flash_loan` callback.
    Reentrant = 14,
    /// The circuit breaker tripped: spot price deviated more than the
    /// configured threshold in a single block.  The pool has been
    /// automatically paused.  Recovery requires the cooldown period to
    /// elapse and a call to `try_circuit_breaker_recovery`, or a direct
    /// admin/governance `unpause`.
    CircuitBreaker = 15,
    /// A fee-on-transfer token deducted more fees than the caller's
    /// `min_received` threshold permitted. The pool received fewer tokens
    /// than requested; the call is reverted to protect the caller.
    FotSlippage = 16,
    /// Spot price deviated beyond the configured oracle tolerance.
    OracleDeviationExceeded = 17,
    /// Flash-loan receiver did not return the borrowed amounts plus fees.
    FlashLoanRepaymentFailed = 18,
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
    fn on_flash_loan(
        env: Env,
        token_a_amount: i128,
        token_b_amount: i128,
        fee_a: i128,
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
        env.storage()
            .instance()
            .set(&DataKey::MultisigQuorum, &0_u32);
        env.storage().instance().set(
            &DataKey::MultisigSigners,
            &soroban_sdk::Vec::<Address>::new(&env),
        );
        // Issue #294: minimum liquidity not yet locked.
        env.storage()
            .instance()
            .set(&DataKey::MinLiquidityLocked, &false);
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

        // If a k-of-n multisig guard is configured, the single-admin path must
        // not be allowed to drain reserves. Callers must use the multisig flow
        // (propose_emergency_withdraw / exec_multisig_emergency_wd) instead.
        let quorum: u32 = env
            .storage()
            .instance()
            .get(&DataKey::MultisigQuorum)
            .unwrap_or(0);
        if quorum > 0 {
            return Err(AmmError::Unauthorized);
        }

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
            SepTokenClient::new(&env, &token_a).transfer(
                &env.current_contract_address(),
                &to,
                &reserve_a,
            );
        }
        if reserve_b > 0 {
            SepTokenClient::new(&env, &token_b).transfer(
                &env.current_contract_address(),
                &to,
                &reserve_b,
            );
        }

        // Zero out reserves
        env.storage().instance().set(&DataKey::ReserveA, &0_i128);
        env.storage().instance().set(&DataKey::ReserveB, &0_i128);

        // Emit event for audit trail
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "emergency_withdraw"), admin.clone()),
            (to, reserve_a, reserve_b)
        );

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

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "cb_config"),),
            (threshold_bps, cooldown_secs)
        );

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

            soroban_amm_sdk::emit_versioned_event!(
                env,
                (Symbol::new(env, "circuit_break"),),
                (baseline_price, current_price, deviation_bps, threshold_bps)
            );

            return Err(AmmError::CircuitBreaker);
        }

        Ok(())
    }

    /// Update the protocol fee configuration. Admin-only.
    ///
    /// Set `protocol_fee_bps` to 0 to disable protocol fee collection.
    /// `protocol_fee_bps` must be ≤ the pool's `fee_bps`.
    pub fn set_protocol_fee(
        env: Env,
        admin: Address,
        recipient: Address,
        protocol_fee_bps: i128,
    ) -> Result<(), AmmError> {
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
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "lp_rebate_set"), admin),
            (lp_rebate_bps,)
        );
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
        env.storage().instance().set(
            &DataKey::MultisigProposalRecipient,
            &Option::<Address>::None,
        );
        env.storage().instance().set(
            &DataKey::MultisigProposalApprovals,
            &soroban_sdk::Vec::<Address>::new(&env),
        );
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "multisig_set"), admin),
            (quorum,)
        );
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
        env.storage().instance().set(
            &DataKey::MultisigProposalRecipient,
            &Some(recipient.clone()),
        );
        env.storage()
            .instance()
            .set(&DataKey::MultisigProposalApprovals, &approvals);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "ms_proposed"), signer),
            (recipient, approvals.len())
        );
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
    pub fn exec_multisig_emergency_wd(env: Env, signer: Address) -> Result<(), AmmError> {
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
        if approvals.len() < quorum {
            return Err(AmmError::InsufficientShares);
        }
        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        let reserve_a = Self::get_reserve_a(env.clone());
        let reserve_b = Self::get_reserve_b(env.clone());
        if reserve_a > 0 {
            SepTokenClient::new(&env, &token_a).transfer(
                &env.current_contract_address(),
                &to,
                &reserve_a,
            );
        }
        if reserve_b > 0 {
            SepTokenClient::new(&env, &token_b).transfer(
                &env.current_contract_address(),
                &to,
                &reserve_b,
            );
        }
        env.storage().instance().set(&DataKey::ReserveA, &0_i128);
        env.storage().instance().set(&DataKey::ReserveB, &0_i128);
        // Clear proposal.
        env.storage().instance().set(
            &DataKey::MultisigProposalRecipient,
            &Option::<Address>::None,
        );
        env.storage().instance().set(
            &DataKey::MultisigProposalApprovals,
            &soroban_sdk::Vec::<Address>::new(&env),
        );
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "ms_ew"), signer),
            (to, reserve_a, reserve_b)
        );
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

        let agg =
            OracleAggregatorClient::new(env, &oracle_addr).get_price_safe(token_in, token_out);
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
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "flash_fee_upd"), admin.clone()),
            (new_fee_bps,)
        );
        Ok(())
    }

    /// Nominate a new admin. The nominee must call `accept_admin` to complete the transfer.
    ///
    /// # Panics
    /// - If `current_admin` is not the stored admin.
    /// - If `current_admin` auth fails.
    pub fn propose_admin(
        env: Env,
        current_admin: Address,
        new_admin: Address,
    ) -> Result<(), AmmError> {
        let stored: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        if current_admin != stored {
            return Err(AmmError::Unauthorized);
        }
        current_admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &Some(new_admin.clone()));
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "admin_nominated"),),
            (current_admin, new_admin)
        );
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
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "admin_changed"),),
            (new_admin,)
        );
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
    /// TWAL must run first so it reads the old `LastTimestamp` before TWAP updates it.
    /// Returns the pre-checkpoint reserves (same as calling `checkpoint_twap` directly).
    fn checkpoint_oracles(env: &Env) -> (i128, i128) {
        Self::checkpoint_twal(env);
        Self::checkpoint_twap(env)
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

        let (reserve_a, reserve_b) = Self::checkpoint_oracles(&env);

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

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "add_liquidity"), provider),
            (amount_a, amount_b, shares_to_provider)
        );

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

        let (reserve_a, reserve_b) = Self::checkpoint_oracles(&env);

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

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (symbol_short!("rm_liq"),),
            (provider.clone(), shares, out_a, out_b)
        );

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
        let (reserve_a, reserve_b) = Self::checkpoint_oracles(&env);

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

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (symbol_short!("rm_liq_1s"),),
            (provider.clone(), shares, token_out.clone(), total_out)
        );

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

        let (reserve_a, reserve_b) = Self::checkpoint_oracles(&env);

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
            env.storage().instance().set(
                &DataKey::ReserveA,
                &(reserve_in + amount_in - net_protocol_fee),
            );
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
            env.storage().instance().set(
                &DataKey::ReserveB,
                &(reserve_in + amount_in - net_protocol_fee),
            );
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

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "swap"), trader),
            (token_in, amount_in, token_out, amount_out)
        );

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

        let (reserve_a, reserve_b) = Self::checkpoint_oracles(&env);

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
            env.storage().instance().set(
                &DataKey::ReserveA,
                &(reserve_a + amount_in - net_protocol_fee),
            );
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
            env.storage().instance().set(
                &DataKey::ReserveB,
                &(reserve_b + amount_in - net_protocol_fee),
            );
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

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "swap"), trader),
            (token_in, amount_in, token_out, amount_out)
        );

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
        amount_a: i128,
        amount_b: i128,
        data: Bytes,
    ) -> Result<(i128, i128), AmmError> {
        if Self::is_paused(env.clone()) {
            return Err(AmmError::Paused);
        }
        if amount_a < 0 || amount_b < 0 || (amount_a == 0 && amount_b == 0) {
            return Err(AmmError::ZeroAmount);
        }

        let (reserve_a, reserve_b) = Self::checkpoint_oracles(&env);
        // Acquire the reentrancy lock before any external call.
        // This prevents the receiver's on_flash_loan callback from calling
        // back into swap / add_liquidity / remove_liquidity / flash_loan.
        Self::enter_lock(&env)?;

        // Circuit breaker check before borrowing funds.
        Self::check_circuit_breaker(&env)?;

        let token_a: Address = env.storage().instance().get(&DataKey::TokenA).unwrap();
        let token_b: Address = env.storage().instance().get(&DataKey::TokenB).unwrap();
        if reserve_a < amount_a || reserve_b < amount_b {
            return Err(AmmError::InsufficientLiquidity);
        }

        let fee_bps = Self::get_flash_loan_fee_bps(env.clone());
        let fee_a = if fee_bps > 0 && amount_a > 0 {
            (amount_a * fee_bps / 10_000).max(1)
        } else {
            0
        };
        let fee_b = if fee_bps > 0 && amount_b > 0 {
            (amount_b * fee_bps / 10_000).max(1)
        } else {
            0
        };
        let pool = env.current_contract_address();
        let token_a_client = SepTokenClient::new(&env, &token_a);
        let token_b_client = SepTokenClient::new(&env, &token_b);
        let balance_a_before = token_a_client.balance(&pool);
        let balance_b_before = token_b_client.balance(&pool);

        if amount_a > 0 {
            token_a_client.transfer(&pool, &receiver, &amount_a);
        }
        if amount_b > 0 {
            token_b_client.transfer(&pool, &receiver, &amount_b);
        }

        // ── External callback (lock is held) ─────────────────────────────────
        // The receiver cannot reenter this pool because Locked == true.
        // Any reentrant call will return AmmError::Reentrant.
        let accepted = FlashLoanReceiverClient::new(&env, &receiver)
            .on_flash_loan(&amount_a, &amount_b, &fee_a, &fee_b, &data);
        if !accepted {
            return Err(AmmError::FlashLoanRepaymentFailed);
        }

        let balance_a_after = token_a_client.balance(&pool);
        let balance_b_after = token_b_client.balance(&pool);
        if balance_a_after < balance_a_before + fee_a || balance_b_after < balance_b_before + fee_b
        {
            return Err(AmmError::FlashLoanRepaymentFailed);
        }

        let accrued_fee_a = env
            .storage()
            .instance()
            .get(&DataKey::AccruedFeeA)
            .unwrap_or(0);
        let accrued_fee_b = env
            .storage()
            .instance()
            .get(&DataKey::AccruedFeeB)
            .unwrap_or(0);
        let reserve_a_after = balance_a_after - accrued_fee_a;
        let reserve_b_after = balance_b_after - accrued_fee_b;
        env.storage()
            .instance()
            .set(&DataKey::ReserveA, &reserve_a_after);
        env.storage()
            .instance()
            .set(&DataKey::ReserveB, &reserve_b_after);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "flash_loan"), receiver),
            (amount_a, amount_b, fee_a, fee_b)
        );

        // Release the lock only on the success path; on error paths Soroban
        // reverts all storage writes (including the lock) automatically.
        Self::exit_lock(&env);
        Ok((fee_a, fee_b))
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
    pub fn simulate_swap(
        env: Env,
        token_in: Address,
        amount_in: i128,
    ) -> Result<SwapSimulation, AmmError> {
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
            env.storage().instance().set(
                &DataKey::ReserveA,
                &(reserve_in + actual_received - protocol_fee),
            );
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
            env.storage().instance().set(
                &DataKey::ReserveB,
                &(reserve_in + actual_received - protocol_fee),
            );
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
            soroban_amm_sdk::emit_versioned_event!(
                env,
                (Symbol::new(&env, "fot_detected"), token_in.clone()),
                (amount_in, actual_received)
            );
        }

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "swap"), trader),
            (token_in, actual_received, token_out, amount_out, referrer)
        );

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

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "add_liquidity"), provider),
            (actual_a, actual_b, shares)
        );

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
pub(crate) mod tests;

// ── Property-based tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod prop_tests;
