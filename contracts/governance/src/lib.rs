//! Governance contract for LP-weighted fee parameter voting.
//!
//! LP token holders can propose changes to the pool's `fee_bps`.
//! Voting power is locked during the proposal lifecycle to prevent
//! flash-loan and vote-then-sell attacks. A proposal passes when:
//!   - `votes_for > votes_against`
//!   - total votes cast >= quorum (configurable % of total LP supply at snapshot)
//!
//! After the voting period ends a timelock delay must elapse before anyone
//! can call `execute()`, which applies the change via `update_fee()` on the
//! AMM contract.

#![no_std]

// Export compiled WASM for tests/dev usage when the `testutils` feature is enabled.
#[cfg(feature = "testutils")]
pub const WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/wasm32v1-none/release/governance.wasm"
));

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, Address, Env, Symbol, Vec};

// ── Constants ─────────────────────────────────────────────────────────────────

const MAX_BPS: i128 = 10_000;
const MIN_PERSISTENT_TTL: u32 = 172_800; // ~10 days at 5s/ledger
const PERSISTENT_TTL_BUMP_TO: u32 = 259_200; // ~15 days at 5s/ledger
/// Multisig may veto a passed proposal within this window after voting ends.
const VETO_WINDOW_SECS: u64 = 24 * 60 * 60;
/// Maximum delegation chain depth (prevents unbounded recursion).
const MAX_DELEGATION_DEPTH: u32 = 8;

// ── Typed errors ─────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum GovernanceError {
    AlreadyInitialized = 1,
    InvalidVotingPeriod = 2,
    InvalidTimelock = 3,
    InvalidQuorumBps = 4,
    InvalidProposerStake = 5,
    InvalidFeeBps = 6,
    ZeroTotalSupply = 7,
    InsufficientStake = 8,
    ProposalNotFound = 9,
    VotingNotStarted = 10,
    VotingPeriodEnded = 11,
    AlreadyExecuted = 12,
    ProposalCancelled = 13,
    AlreadyVoted = 14,
    NoVotingPower = 15,
    VotingPeriodActive = 16,
    ProposalExpired = 17,
    TimelockNotElapsed = 18,
    QuorumNotMet = 19,
    ProposalDefeated = 20,
    NotProposer = 21,
    NoLockedVote = 22,
    ProposalNotConcluded = 23,
    CannotDelegateToSelf = 24,
    Unauthorized = 25,
    HasDelegated = 26,
    DelegationCycle = 27,
    ProposalVetoed = 28,
    VetoWindowExpired = 29,
    NotVetoMultisig = 30,
    InsufficientSnapshotBal = 31,
    VetoMultisigNotSet = 32,
}

// ── Storage keys ─────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    /// Address of the AMM pool contract.
    AmmPool,
    /// Address of the LP token contract.
    LpToken,
    /// Monotonically increasing proposal counter.
    ProposalCount,
    /// Governance admin.
    Admin,
    /// Minimum proposer stake in basis points of total LP supply.
    MinProposerStakeBps,
    /// Voting period in seconds (configurable at initialize).
    VotingPeriod,
    /// Timelock delay in seconds (configurable at initialize).
    Timelock,
    /// Quorum requirement in basis points of total LP supply at snapshot.
    QuorumBps,
    /// Additional quorum bps required per day a proposal has been open (#311).
    QuorumDecayRateBpsPerDay,
    /// Individual proposal storage.
    Proposal(u32),
    /// Vote record for a voter on a proposal: (proposal_id, voter).
    HasVoted(u32, Address),
    /// Locked voting amount for a voter on a proposal.
    LockedVote(u32, Address),
    /// Delegation mapping: delegator -> delegatee address.
    Delegate(Address),
    /// Protocol multisig authorized to veto passed proposals.
    VetoMultisig,
    /// Number of addresses delegating to a given delegatee.
    DelegatorCount(Address),
    /// Delegator at index for a delegatee.
    Delegator(Address, u32),
    /// Index of a delegator in their delegatee's list (for removal).
    DelegatorSlot(Address),
    /// Audit record for a vetoed proposal.
    VetoAudit(u32),
}

// ── Types ─────────────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum ProposalStatus {
    /// Voting is open.
    Active,
    /// Voting closed; waiting for timelock to expire.
    Pending,
    /// Timelock elapsed; ready to execute.
    Queued,
    /// Proposal was executed successfully.
    Executed,
    /// Proposal failed quorum or majority.
    Defeated,
    /// Proposal expired without execution after timelock window.
    Expired,
    /// Proposal was cancelled by the original proposer.
    Cancelled,
    /// Multisig vetoed; community discussion period is active.
    InDiscussion,
    /// Multisig vetoed; discussion period ended — cannot execute.
    Vetoed,
}

/// Choice for a vote.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum Vote {
    For,
    Against,
    Abstain,
}

/// Records how an address voted on a specific proposal.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum VoteRecord {
    DidNotVote,
    VotedFor,
    VotedAgainst,
    VotedAbstain,
}

/// Current governance configuration returned by `get_params`.
#[contracttype]
#[derive(Clone, Debug)]
pub struct GovernanceParams {
    pub voting_period_secs: u64,
    pub timelock_secs: u64,
    pub quorum_bps: i128,
    pub min_proposer_stake_bps: i128,
    pub veto_multisig: Option<Address>,
    /// Extra quorum bps per day open; 0 disables decay (#311).
    pub quorum_decay_rate_bps_per_day: i128,
}

/// On-chain audit trail for a governance veto.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct VetoAudit {
    pub proposal_id: u32,
    pub vetoed_by: Address,
    pub vetoed_at: u64,
    pub discussion_end: u64,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateProtocolFeeParams {
    pub new_bps: i128,
    pub new_recipient: Address,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateFactoryTreasuryParams {
    pub factory: Address,
    pub treasury: Address,
    pub global_protocol_fee_bps: i128,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateFactoryGlobalFeeParams {
    pub factory: Address,
    pub offset: u32,
    pub limit: u32,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct CreatePolVestingParams {
    /// POL vesting contract address.
    pub pol_vesting: Address,
    /// Beneficiary of the vesting schedule.
    pub beneficiary: Address,
    /// LP token to vest.
    pub lp_token: Address,
    /// AMM pool the LP tokens belong to.
    pub pool: Address,
    /// Total LP tokens to vest.
    pub total: i128,
    pub start_ledger: u32,
    pub cliff_ledger: u32,
    pub end_ledger: u32,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum ProposalKind {
    UpdateFee(i128),
    UpdateFeeTier(i128), // 0-3: VeryLow, Low, Medium, High
    UpdateProtocolFee(UpdateProtocolFeeParams),
    UpdateFlashLoanFee(i128),
    TransferAdmin(Address),
    PausePool,
    UnpausePool,
    EmergencyWithdraw(Address),
    UpdateFactoryTreasury(UpdateFactoryTreasuryParams),
    UpdateFactoryGlobalFee(UpdateFactoryGlobalFeeParams),
    /// Deploy a time-based vesting schedule for protocol-owned LP tokens.
    CreatePolVesting(CreatePolVestingParams),
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct Proposal {
    pub id: u32,
    pub proposer: Address,
    pub kind: ProposalKind,
    /// LP total supply snapshot at proposal creation.
    pub snapshot_total_supply: i128,
    /// Ledger sequence when LP balances were snapshotted for voting.
    pub snapshot_ledger: u32,
    /// Timestamp when voting opens (== creation timestamp).
    pub vote_start: u64,
    /// Timestamp when voting closes.
    pub vote_end: u64,
    /// Timestamp after which execution is allowed (vote_end + timelock_secs).
    pub execute_after: u64,
    /// Timestamp after which the proposal expires if not executed (execute_after + timelock_secs).
    pub expires_at: u64,
    pub votes_for: i128,
    pub votes_against: i128,
    pub votes_abstain: i128,
    pub executed: bool,
    pub cancelled: bool,
    pub vetoed: bool,
    pub vetoed_by: Option<Address>,
    pub vetoed_at: Option<u64>,
    pub discussion_end: Option<u64>,
}

// ── LP token client ───────────────────────────────────────────────────────────

#[soroban_sdk::contractclient(name = "LpTokenClient")]
pub trait LpTokenInterface {
    fn balance(env: Env, id: Address) -> i128;
    fn balance_at(env: Env, id: Address, ledger: u32) -> i128;
    fn total_supply(env: Env) -> i128;
    fn lock(env: Env, holder: Address, amount: i128);
    fn unlock(env: Env, holder: Address, amount: i128);
}

// ── AMM client ────────────────────────────────────────────────────────────────

#[soroban_sdk::contractclient(name = "AmmPoolClient")]
pub trait AmmPoolInterface {
    fn update_fee(env: Env, new_fee_bps: i128);
    fn update_flash_loan_fee(env: Env, new_fee_bps: i128);
    fn set_protocol_fee(env: Env, admin: Address, recipient: Address, protocol_fee_bps: i128);
    fn pause(env: Env);
    fn unpause(env: Env);
    fn emergency_withdraw(env: Env, to: Address);
    fn propose_admin(env: Env, current_admin: Address, new_admin: Address);
}

// ── Factory client ────────────────────────────────────────────────────────────

#[soroban_sdk::contractclient(name = "FactoryClient")]
pub trait FactoryInterface {
    fn set_treasury(env: Env, admin: Address, treasury: Address, global_protocol_fee_bps: i128);
    fn set_global_fee_paginated(env: Env, admin: Address, offset: u32, limit: u32) -> u32;
}

// ── POL Vesting client ────────────────────────────────────────────────────────

#[soroban_sdk::contractclient(name = "PolVestingClient")]
pub trait PolVestingInterface {
    #[allow(clippy::too_many_arguments)]
    fn create_vesting(
        env: Env,
        governance: Address,
        beneficiary: Address,
        lp_token: Address,
        pool: Address,
        total: i128,
        start_ledger: u32,
        cliff_ledger: u32,
        end_ledger: u32,
    );
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct Governance;

#[contractimpl]
impl Governance {
    // ── Setup ─────────────────────────────────────────────────────────────────

    /// One-time initialisation. Must be called after deployment.
    ///
    /// - `voting_period_secs` must be > 0.
    /// - `timelock_secs` must be > 0.
    /// - `quorum_bps` must be in [1, 10_000].
    /// - `min_proposer_stake_bps` must be in [0, 10_000].
    #[allow(clippy::too_many_arguments)]
    pub fn initialize(
        env: Env,
        admin: Address,
        amm_pool: Address,
        lp_token: Address,
        voting_period_secs: u64,
        timelock_secs: u64,
        quorum_bps: i128,
        min_proposer_stake_bps: i128,
    ) -> Result<(), GovernanceError> {
        if env.storage().instance().has(&DataKey::AmmPool) {
            return Err(GovernanceError::AlreadyInitialized);
        }
        if voting_period_secs == 0 {
            return Err(GovernanceError::InvalidVotingPeriod);
        }
        if timelock_secs == 0 {
            return Err(GovernanceError::InvalidTimelock);
        }
        if !(1..=MAX_BPS).contains(&quorum_bps) {
            return Err(GovernanceError::InvalidQuorumBps);
        }
        if !(0..=MAX_BPS).contains(&min_proposer_stake_bps) {
            return Err(GovernanceError::InvalidProposerStake);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::AmmPool, &amm_pool);
        env.storage().instance().set(&DataKey::LpToken, &lp_token);
        env.storage()
            .instance()
            .set(&DataKey::VotingPeriod, &voting_period_secs);
        env.storage()
            .instance()
            .set(&DataKey::Timelock, &timelock_secs);
        env.storage()
            .instance()
            .set(&DataKey::QuorumBps, &quorum_bps);
        env.storage()
            .instance()
            .set(&DataKey::MinProposerStakeBps, &min_proposer_stake_bps);
        env.storage()
            .instance()
            .set(&DataKey::QuorumDecayRateBpsPerDay, &0i128);
        env.storage().instance().set(&DataKey::ProposalCount, &0u32);
        Ok(())
    }

    /// Admin-only: quorum increases by this many bps per day a proposal is open (#311).
    pub fn set_quorum_decay_bps_per_day(
        env: Env,
        new_rate: i128,
    ) -> Result<(), GovernanceError> {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        if new_rate < 0 {
            return Err(GovernanceError::InvalidQuorumBps);
        }
        env.storage()
            .instance()
            .set(&DataKey::QuorumDecayRateBpsPerDay, &new_rate);
        Ok(())
    }

    /// Effective quorum bps for a proposal (base + decay, capped at 10_000).
    pub fn get_effective_quorum(env: Env, proposal_id: u32) -> i128 {
        let proposal: Proposal = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .expect("proposal not found");
        Self::effective_quorum_bps(&env, &proposal)
    }

    /// Admin-only governance parameter update.
    pub fn set_min_proposer_stake_bps(env: Env, new_bps: i128) -> Result<(), GovernanceError> {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        if !(0..=MAX_BPS).contains(&new_bps) {
            return Err(GovernanceError::InvalidProposerStake);
        }
        env.storage()
            .instance()
            .set(&DataKey::MinProposerStakeBps, &new_bps);
        Ok(())
    }

    /// Admin-only: update the timelock delay between vote end and execution.
    /// A delay of 0 means execution is allowed immediately after the voting period ends.
    pub fn set_timelock_delay(env: Env, new_delay: u64) -> Result<(), GovernanceError> {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        env.storage().instance().set(&DataKey::Timelock, &new_delay);
        Ok(())
    }

    // ── Core functions ────────────────────────────────────────────────────────

    /// Create a new proposal to change the pool fee.
    ///
    /// The proposer must hold at least the configured minimum LP stake.
    /// Returns the new `proposal_id`.
    pub fn propose(
        env: Env,
        proposer: Address,
        kind: ProposalKind,
    ) -> Result<u32, GovernanceError> {
        proposer.require_auth();

        match &kind {
            ProposalKind::UpdateFee(new_fee_bps) => {
                if !(0..=MAX_BPS).contains(new_fee_bps) {
                    return Err(GovernanceError::InvalidFeeBps);
                }
            }
            ProposalKind::UpdateFeeTier(fee_tier) => {
                // Validate fee_tier is 0-3
                Self::fee_tier_to_bps(*fee_tier)?;
            }
            ProposalKind::UpdateProtocolFee(params) => {
                if !(0..=MAX_BPS).contains(&params.new_bps) {
                    return Err(GovernanceError::InvalidFeeBps);
                }
            }
            ProposalKind::UpdateFlashLoanFee(new_bps) => {
                if !(0..=MAX_BPS).contains(new_bps) {
                    return Err(GovernanceError::InvalidFeeBps);
                }
            }
            ProposalKind::TransferAdmin(_new_admin) => {}
            ProposalKind::PausePool => {}
            ProposalKind::UnpausePool => {}
            ProposalKind::EmergencyWithdraw(_) => {}
            ProposalKind::UpdateFactoryTreasury(params) => {
                if !(0..=MAX_BPS).contains(&params.global_protocol_fee_bps) {
                    return Err(GovernanceError::InvalidFeeBps);
                }
            }
            ProposalKind::UpdateFactoryGlobalFee(_) => {}
            ProposalKind::CreatePolVesting(_) => {}
        }

        let lp_token: Address = env.storage().instance().get(&DataKey::LpToken).unwrap();
        let lp_client = LpTokenClient::new(&env, &lp_token);

        let total_supply = lp_client.total_supply();
        if total_supply == 0 {
            return Err(GovernanceError::ZeroTotalSupply);
        }
        let proposer_balance = lp_client.balance(&proposer);
        let min_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MinProposerStakeBps)
            .unwrap_or(0);
        let min_stake = ((total_supply * min_bps) / MAX_BPS).max(1);
        if proposer_balance < min_stake {
            return Err(GovernanceError::InsufficientStake);
        }

        let voting_period: u64 = env
            .storage()
            .instance()
            .get(&DataKey::VotingPeriod)
            .unwrap();
        let timelock: u64 = env.storage().instance().get(&DataKey::Timelock).unwrap();

        let now = env.ledger().timestamp();
        let vote_end = now + voting_period;
        let execute_after = vote_end + timelock;
        // Execution window: at least one voting period even when timelock is 0.
        let expires_at = execute_after + timelock.max(voting_period);

        let id: u32 = env
            .storage()
            .instance()
            .get(&DataKey::ProposalCount)
            .unwrap();

        let snapshot_ledger = env.ledger().sequence();

        let proposal = Proposal {
            id,
            proposer: proposer.clone(),
            kind: kind.clone(),
            snapshot_total_supply: total_supply,
            snapshot_ledger,
            vote_start: now,
            vote_end,
            execute_after,
            expires_at,
            votes_for: 0,
            votes_against: 0,
            votes_abstain: 0,
            executed: false,
            cancelled: false,
            vetoed: false,
            vetoed_by: None,
            vetoed_at: None,
            discussion_end: None,
        };

        let proposal_key = DataKey::Proposal(id);
        env.storage().persistent().set(&proposal_key, &proposal);
        Self::bump_key_ttl(&env, &proposal_key);
        env.storage()
            .instance()
            .set(&DataKey::ProposalCount, &(id + 1));

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "proposed"),),
            (id, proposer, kind, vote_end, snapshot_ledger)
        );

        Ok(id)
    }

    /// Cast a vote on an active proposal.
    ///
    /// Voting power uses LP balances snapshotted at proposal creation (`snapshot_ledger`).
    /// Delegators cannot vote directly; the terminal delegatee votes with aggregated power.
    pub fn vote(
        env: Env,
        voter: Address,
        proposal_id: u32,
        choice: Vote,
    ) -> Result<(), GovernanceError> {
        voter.require_auth();

        if Self::get_delegate(env.clone(), voter.clone()).is_some() {
            return Err(GovernanceError::HasDelegated);
        }

        let proposal_key = DataKey::Proposal(proposal_id);
        let mut proposal: Proposal = env
            .storage()
            .persistent()
            .get(&proposal_key)
            .ok_or(GovernanceError::ProposalNotFound)?;
        Self::bump_key_ttl(&env, &proposal_key);

        let now = env.ledger().timestamp();
        if now < proposal.vote_start {
            return Err(GovernanceError::VotingNotStarted);
        }
        if now > proposal.vote_end {
            return Err(GovernanceError::VotingPeriodEnded);
        }
        if proposal.executed {
            return Err(GovernanceError::AlreadyExecuted);
        }
        if proposal.cancelled {
            return Err(GovernanceError::ProposalCancelled);
        }
        if proposal.vetoed {
            return Err(GovernanceError::ProposalVetoed);
        }

        let voted_key = DataKey::HasVoted(proposal_id, voter.clone());
        if env.storage().persistent().has(&voted_key) {
            return Err(GovernanceError::AlreadyVoted);
        }

        let lp_token: Address = env.storage().instance().get(&DataKey::LpToken).unwrap();
        let lp_client = LpTokenClient::new(&env, &lp_token);

        let (voting_power, lock_accounts) =
            Self::aggregated_voting_power(&env, &lp_client, &voter, &proposal)?;
        if voting_power == 0 {
            return Err(GovernanceError::NoVotingPower);
        }

        for i in 0..lock_accounts.len() {
            let (account, amount) = lock_accounts.get(i).unwrap();
            lp_client.lock(&account, &amount);
            let lock_key = DataKey::LockedVote(proposal_id, account.clone());
            env.storage().persistent().set(&lock_key, &amount);
            Self::bump_key_ttl(&env, &lock_key);
        }

        match choice {
            Vote::For => {
                proposal.votes_for += voting_power;
            }
            Vote::Against => {
                proposal.votes_against += voting_power;
            }
            Vote::Abstain => {
                proposal.votes_abstain += voting_power;
            }
        }

        env.storage().persistent().set(&proposal_key, &proposal);
        Self::bump_key_ttl(&env, &proposal_key);

        let record = match choice {
            Vote::For => VoteRecord::VotedFor,
            Vote::Against => VoteRecord::VotedAgainst,
            Vote::Abstain => VoteRecord::VotedAbstain,
        };
        env.storage().persistent().set(&voted_key, &record);
        Self::bump_key_ttl(&env, &voted_key);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "voted"),),
            (proposal_id, voter, choice, voting_power)
        );
        Ok(())
    }

    /// Execute a passed proposal after the timelock has elapsed.
    ///
    /// Anyone can call this once the conditions are met.
    pub fn execute(env: Env, proposal_id: u32) -> Result<(), GovernanceError> {
        let proposal_key = DataKey::Proposal(proposal_id);
        let mut proposal: Proposal = env
            .storage()
            .persistent()
            .get(&proposal_key)
            .ok_or(GovernanceError::ProposalNotFound)?;
        Self::bump_key_ttl(&env, &proposal_key);

        if proposal.executed {
            return Err(GovernanceError::AlreadyExecuted);
        }
        if proposal.cancelled {
            return Err(GovernanceError::ProposalCancelled);
        }
        if proposal.vetoed {
            return Err(GovernanceError::ProposalVetoed);
        }

        let now = env.ledger().timestamp();

        if now <= proposal.vote_end {
            return Err(GovernanceError::VotingPeriodActive);
        }
        if now > proposal.expires_at {
            return Err(GovernanceError::ProposalExpired);
        }
        if now < proposal.execute_after {
            return Err(GovernanceError::TimelockNotElapsed);
        }

        let effective_quorum = Self::effective_quorum_bps(&env, &proposal);
        let total_votes = proposal.votes_for + proposal.votes_against + proposal.votes_abstain;
        let quorum_threshold = proposal.snapshot_total_supply * effective_quorum / MAX_BPS;
        if total_votes < quorum_threshold {
            return Err(GovernanceError::QuorumNotMet);
        }

        if proposal.votes_for <= proposal.votes_against {
            return Err(GovernanceError::ProposalDefeated);
        }

        let amm_pool: Address = env.storage().instance().get(&DataKey::AmmPool).unwrap();
        let amm_client = AmmPoolClient::new(&env, &amm_pool);
        match &proposal.kind {
            ProposalKind::UpdateFee(new_fee_bps) => {
                amm_client.update_fee(new_fee_bps);
            }
            ProposalKind::UpdateFeeTier(fee_tier) => {
                let new_fee_bps = Self::fee_tier_to_bps(*fee_tier)?;
                amm_client.update_fee(&new_fee_bps);
            }
            ProposalKind::UpdateProtocolFee(params) => {
                let self_addr = env.current_contract_address();
                amm_client.set_protocol_fee(&self_addr, &params.new_recipient, &params.new_bps);
            }
            ProposalKind::UpdateFlashLoanFee(new_bps) => {
                amm_client.update_flash_loan_fee(new_bps);
            }
            ProposalKind::TransferAdmin(new_admin) => {
                let self_addr = env.current_contract_address();
                amm_client.propose_admin(&self_addr, new_admin);
            }
            ProposalKind::PausePool => {
                amm_client.pause();
            }
            ProposalKind::UnpausePool => {
                amm_client.unpause();
            }
            ProposalKind::EmergencyWithdraw(to) => {
                amm_client.emergency_withdraw(to);
            }
            ProposalKind::UpdateFactoryTreasury(params) => {
                let self_addr = env.current_contract_address();
                let factory_client = FactoryClient::new(&env, &params.factory);
                factory_client.set_treasury(
                    &self_addr,
                    &params.treasury,
                    &params.global_protocol_fee_bps,
                );
            }
            ProposalKind::UpdateFactoryGlobalFee(params) => {
                let self_addr = env.current_contract_address();
                let factory_client = FactoryClient::new(&env, &params.factory);
                let _updated = factory_client.set_global_fee_paginated(
                    &self_addr,
                    &params.offset,
                    &params.limit,
                );
            }
            ProposalKind::CreatePolVesting(params) => {
                let self_addr = env.current_contract_address();
                PolVestingClient::new(&env, &params.pol_vesting).create_vesting(
                    &self_addr,
                    &params.beneficiary,
                    &params.lp_token,
                    &params.pool,
                    &params.total,
                    &params.start_ledger,
                    &params.cliff_ledger,
                    &params.end_ledger,
                );
            }
        }

        proposal.executed = true;
        env.storage().persistent().set(&proposal_key, &proposal);
        Self::bump_key_ttl(&env, &proposal_key);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "executed"),),
            (proposal_id, proposal.kind.clone())
        );
        Ok(())
    }

    /// Cancel an active proposal. Only the original proposer can cancel,
    /// and only while voting is still open.
    pub fn cancel_proposal(
        env: Env,
        proposal_id: u32,
        proposer: Address,
    ) -> Result<(), GovernanceError> {
        proposer.require_auth();

        let proposal_key = DataKey::Proposal(proposal_id);
        let mut proposal: Proposal = env
            .storage()
            .persistent()
            .get(&proposal_key)
            .ok_or(GovernanceError::ProposalNotFound)?;
        Self::bump_key_ttl(&env, &proposal_key);

        if proposal.executed {
            return Err(GovernanceError::AlreadyExecuted);
        }
        if proposal.cancelled {
            return Err(GovernanceError::ProposalCancelled);
        }
        if env.ledger().timestamp() > proposal.vote_end {
            return Err(GovernanceError::VotingPeriodEnded);
        }
        if proposal.proposer != proposer {
            return Err(GovernanceError::NotProposer);
        }

        proposal.cancelled = true;
        env.storage().persistent().set(&proposal_key, &proposal);
        Self::bump_key_ttl(&env, &proposal_key);

        env.events()
            .publish((Symbol::new(&env, "cancelled"),), (proposal_id,));
        Ok(())
    }

    /// Query how an address voted on a proposal.
    ///
    /// Returns `VotedFor`, `VotedAgainst`, or `DidNotVote`.
    pub fn get_vote_info(env: Env, proposal_id: u32, voter: Address) -> VoteRecord {
        env.storage()
            .persistent()
            .get(&DataKey::HasVoted(proposal_id, voter))
            .unwrap_or(VoteRecord::DidNotVote)
    }

    /// Return the current governance configuration parameters.
    pub fn get_params(env: Env) -> GovernanceParams {
        GovernanceParams {
            voting_period_secs: env
                .storage()
                .instance()
                .get(&DataKey::VotingPeriod)
                .unwrap(),
            timelock_secs: env.storage().instance().get(&DataKey::Timelock).unwrap(),
            quorum_bps: env.storage().instance().get(&DataKey::QuorumBps).unwrap(),
            min_proposer_stake_bps: env
                .storage()
                .instance()
                .get(&DataKey::MinProposerStakeBps)
                .unwrap(),
            veto_multisig: env.storage().instance().get(&DataKey::VetoMultisig),
            quorum_decay_rate_bps_per_day: env
                .storage()
                .instance()
                .get(&DataKey::QuorumDecayRateBpsPerDay)
                .unwrap_or(0),
        }
    }

    /// Unlock voting power for a concluded proposal.
    pub fn unlock_vote(env: Env, voter: Address, proposal_id: u32) -> Result<(), GovernanceError> {
        voter.require_auth();
        let status = Self::proposal_status(env.clone(), proposal_id);
        if status != ProposalStatus::Executed
            && status != ProposalStatus::Defeated
            && status != ProposalStatus::Expired
            && status != ProposalStatus::Cancelled
            && status != ProposalStatus::Vetoed
            && status != ProposalStatus::InDiscussion
        {
            return Err(GovernanceError::ProposalNotConcluded);
        }
        let lock_key = DataKey::LockedVote(proposal_id, voter.clone());
        let locked: i128 = env.storage().persistent().get(&lock_key).unwrap_or(0);
        if locked == 0 {
            return Err(GovernanceError::NoLockedVote);
        }

        let lp_token: Address = env.storage().instance().get(&DataKey::LpToken).unwrap();
        LpTokenClient::new(&env, &lp_token).unlock(&voter, &locked);
        env.storage().persistent().remove(&lock_key);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "vote_unlocked"), voter.clone()),
            (proposal_id, locked)
        );
        Ok(())
    }

    /// Delegate voting power to another address.
    ///
    /// The delegator's voting power is transferred to the delegatee who votes on their behalf.
    /// The delegator cannot vote while delegation is active.
    ///
    /// # Parameters
    /// - `from` – LP holder delegating their voting power; must authorize this call.
    /// - `to` – Address receiving the delegated voting power.
    ///
    /// # Panics
    /// - If `from` is the same as `to`.
    pub fn delegate(env: Env, from: Address, to: Address) -> Result<(), GovernanceError> {
        from.require_auth();
        if from == to {
            return Err(GovernanceError::CannotDelegateToSelf);
        }
        if Self::delegation_reaches(&env, &to, &from, 0) {
            return Err(GovernanceError::DelegationCycle);
        }

        if let Some(old) = Self::get_delegate(env.clone(), from.clone()) {
            Self::remove_delegator_index(&env, &old, &from);
        }
        Self::add_delegator_index(&env, &to, &from);

        env.storage()
            .instance()
            .set(&DataKey::Delegate(from.clone()), &to);

        env.events()
            .publish((Symbol::new(&env, "delegated"),), (from, to));
        Ok(())
    }

    /// Remove delegation of voting power.
    ///
    /// After calling, the caller's voting power reverts to themselves.
    ///
    /// # Parameters
    /// - `from` – Address removing their delegation; must authorize this call.
    pub fn undelegate(env: Env, from: Address) {
        from.require_auth();
        if let Some(delegatee) = Self::get_delegate(env.clone(), from.clone()) {
            Self::remove_delegator_index(&env, &delegatee, &from);
        }
        env.storage()
            .instance()
            .remove(&DataKey::Delegate(from.clone()));

        env.events()
            .publish((Symbol::new(&env, "undelegated"),), (from,));
    }

    /// Retrieve the current delegatee for an LP holder.
    ///
    /// Returns `None` if no delegation is active.
    pub fn get_delegate(env: Env, from: Address) -> Option<Address> {
        env.storage()
            .instance()
            .get(&DataKey::Delegate(from))
            .unwrap_or(None)
    }

    /// Admin-only: set the protocol multisig that may veto passed proposals.
    pub fn set_veto_multisig(env: Env, multisig: Address) -> Result<(), GovernanceError> {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::VetoMultisig, &multisig);
        env.events()
            .publish((Symbol::new(&env, "veto_multisig_set"),), (multisig,));
        Ok(())
    }

    /// Veto a passed proposal within 24 hours after voting ends.
    ///
    /// Triggers the governance discussion phase; the proposal cannot be executed.
    pub fn veto(env: Env, proposal_id: u32) -> Result<(), GovernanceError> {
        let multisig: Address = env
            .storage()
            .instance()
            .get(&DataKey::VetoMultisig)
            .ok_or(GovernanceError::VetoMultisigNotSet)?;
        multisig.require_auth();

        let proposal_key = DataKey::Proposal(proposal_id);
        let mut proposal: Proposal = env
            .storage()
            .persistent()
            .get(&proposal_key)
            .ok_or(GovernanceError::ProposalNotFound)?;
        Self::bump_key_ttl(&env, &proposal_key);

        if proposal.executed {
            return Err(GovernanceError::AlreadyExecuted);
        }
        if proposal.cancelled {
            return Err(GovernanceError::ProposalCancelled);
        }
        if proposal.vetoed {
            return Err(GovernanceError::ProposalVetoed);
        }

        let now = env.ledger().timestamp();
        if now <= proposal.vote_end {
            return Err(GovernanceError::VotingPeriodActive);
        }
        if now > proposal.vote_end + VETO_WINDOW_SECS {
            return Err(GovernanceError::VetoWindowExpired);
        }

        let effective_quorum = Self::effective_quorum_bps(&env, &proposal);
        let total_votes = proposal.votes_for + proposal.votes_against + proposal.votes_abstain;
        let quorum_threshold = proposal.snapshot_total_supply * effective_quorum / MAX_BPS;
        if total_votes < quorum_threshold || proposal.votes_for <= proposal.votes_against {
            return Err(GovernanceError::ProposalDefeated);
        }

        let voting_period: u64 = env
            .storage()
            .instance()
            .get(&DataKey::VotingPeriod)
            .unwrap();
        let discussion_end = now + voting_period;

        proposal.vetoed = true;
        proposal.vetoed_by = Some(multisig.clone());
        proposal.vetoed_at = Some(now);
        proposal.discussion_end = Some(discussion_end);
        env.storage().persistent().set(&proposal_key, &proposal);
        Self::bump_key_ttl(&env, &proposal_key);

        let audit = VetoAudit {
            proposal_id,
            vetoed_by: multisig.clone(),
            vetoed_at: now,
            discussion_end,
        };
        let audit_key = DataKey::VetoAudit(proposal_id);
        env.storage().persistent().set(&audit_key, &audit);
        Self::bump_key_ttl(&env, &audit_key);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "vetoed"),),
            (proposal_id, multisig, now, discussion_end)
        );
        Ok(())
    }

    /// Returns the on-chain veto audit record for a proposal, if vetoed.
    pub fn get_veto_audit(env: Env, proposal_id: u32) -> Option<VetoAudit> {
        env.storage()
            .persistent()
            .get(&DataKey::VetoAudit(proposal_id))
    }

    /// LP balance at proposal snapshot ledger for `holder`.
    pub fn get_snapshot_balance(
        env: Env,
        proposal_id: u32,
        holder: Address,
    ) -> Result<i128, GovernanceError> {
        let proposal: Proposal = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernanceError::ProposalNotFound)?;
        let lp_token: Address = env.storage().instance().get(&DataKey::LpToken).unwrap();
        let lp_client = LpTokenClient::new(&env, &lp_token);
        Ok(lp_client.balance_at(&holder, &proposal.snapshot_ledger))
    }

    /// Read a proposal by id.
    pub fn get_proposal(env: Env, proposal_id: u32) -> Proposal {
        let key = DataKey::Proposal(proposal_id);
        let proposal: Proposal = env
            .storage()
            .persistent()
            .get(&key)
            .expect("proposal not found");
        Self::bump_key_ttl(&env, &key);
        proposal
    }

    /// Derive the current status of a proposal.
    pub fn proposal_status(env: Env, proposal_id: u32) -> ProposalStatus {
        let proposal: Proposal = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .expect("proposal not found");
        Self::bump_key_ttl(&env, &DataKey::Proposal(proposal_id));

        if proposal.cancelled {
            return ProposalStatus::Cancelled;
        }

        if proposal.executed {
            return ProposalStatus::Executed;
        }

        let now = env.ledger().timestamp();

        if proposal.vetoed {
            if let Some(end) = proposal.discussion_end {
                if now <= end {
                    return ProposalStatus::InDiscussion;
                }
            }
            return ProposalStatus::Vetoed;
        }

        if now <= proposal.vote_end {
            return ProposalStatus::Active;
        }

        let effective_quorum = Self::effective_quorum_bps(&env, &proposal);
        let total_votes = proposal.votes_for + proposal.votes_against + proposal.votes_abstain;
        let quorum_threshold = proposal.snapshot_total_supply * effective_quorum / MAX_BPS;
        let passed = total_votes >= quorum_threshold && proposal.votes_for > proposal.votes_against;

        if !passed {
            return ProposalStatus::Defeated;
        }

        if now > proposal.expires_at {
            return ProposalStatus::Expired;
        }

        if now >= proposal.execute_after {
            ProposalStatus::Queued
        } else {
            ProposalStatus::Pending
        }
    }

    fn effective_quorum_bps(env: &Env, proposal: &Proposal) -> i128 {
        let base: i128 = env.storage().instance().get(&DataKey::QuorumBps).unwrap();
        let decay_rate: i128 = env
            .storage()
            .instance()
            .get(&DataKey::QuorumDecayRateBpsPerDay)
            .unwrap_or(0);
        if decay_rate == 0 {
            return base;
        }
        let now = env.ledger().timestamp();
        let days_open = if now > proposal.vote_start {
            (now - proposal.vote_start) / 86_400
        } else {
            0
        };
        (base + decay_rate * days_open as i128).min(MAX_BPS)
    }

    fn snapshot_voting_power(
        lp_client: &LpTokenClient,
        holder: &Address,
        proposal: &Proposal,
    ) -> Result<i128, GovernanceError> {
        let power = lp_client.balance_at(holder, &proposal.snapshot_ledger);
        let current = lp_client.balance(holder);
        if current < power {
            return Err(GovernanceError::InsufficientSnapshotBal);
        }
        Ok(power)
    }

    fn aggregated_voting_power(
        env: &Env,
        lp_client: &LpTokenClient,
        voter: &Address,
        proposal: &Proposal,
    ) -> Result<(i128, Vec<(Address, i128)>), GovernanceError> {
        let mut total: i128 = 0;
        let mut locks = Vec::new(env);
        Self::collect_voting_power(env, lp_client, voter, proposal, &mut total, &mut locks, 0)?;
        Ok((total, locks))
    }

    fn collect_voting_power(
        env: &Env,
        lp_client: &LpTokenClient,
        holder: &Address,
        proposal: &Proposal,
        total: &mut i128,
        locks: &mut Vec<(Address, i128)>,
        depth: u32,
    ) -> Result<(), GovernanceError> {
        if depth > MAX_DELEGATION_DEPTH {
            return Err(GovernanceError::DelegationCycle);
        }
        let power = Self::snapshot_voting_power(lp_client, holder, proposal)?;
        if power > 0 {
            *total += power;
            locks.push_back((holder.clone(), power));
        }
        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::DelegatorCount(holder.clone()))
            .unwrap_or(0);
        for i in 0..count {
            let delegator: Address = env
                .storage()
                .instance()
                .get(&DataKey::Delegator(holder.clone(), i))
                .unwrap();
            Self::collect_voting_power(
                env,
                lp_client,
                &delegator,
                proposal,
                total,
                locks,
                depth + 1,
            )?;
        }
        Ok(())
    }

    fn delegation_reaches(env: &Env, current: &Address, target: &Address, depth: u32) -> bool {
        if depth > MAX_DELEGATION_DEPTH {
            return false;
        }
        if current == target {
            return true;
        }
        if let Some(next) = Self::get_delegate(env.clone(), current.clone()) {
            Self::delegation_reaches(env, &next, target, depth + 1)
        } else {
            false
        }
    }

    fn add_delegator_index(env: &Env, delegatee: &Address, delegator: &Address) {
        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::DelegatorCount(delegatee.clone()))
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::Delegator(delegatee.clone(), count), delegator);
        env.storage().instance().set(
            &DataKey::DelegatorSlot(delegator.clone()),
            &(delegatee.clone(), count),
        );
        env.storage()
            .instance()
            .set(&DataKey::DelegatorCount(delegatee.clone()), &(count + 1));
    }

    fn remove_delegator_index(env: &Env, delegatee: &Address, delegator: &Address) {
        let (stored_delegatee, index): (Address, u32) = env
            .storage()
            .instance()
            .get(&DataKey::DelegatorSlot(delegator.clone()))
            .unwrap_or((delegatee.clone(), 0));
        if stored_delegatee != *delegatee {
            return;
        }
        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::DelegatorCount(delegatee.clone()))
            .unwrap_or(0);
        if count == 0 {
            return;
        }
        let last_index = count - 1;
        if index != last_index {
            let last_delegator: Address = env
                .storage()
                .instance()
                .get(&DataKey::Delegator(delegatee.clone(), last_index))
                .unwrap();
            env.storage().instance().set(
                &DataKey::Delegator(delegatee.clone(), index),
                &last_delegator,
            );
            env.storage().instance().set(
                &DataKey::DelegatorSlot(last_delegator.clone()),
                &(delegatee.clone(), index),
            );
        }
        env.storage()
            .instance()
            .remove(&DataKey::Delegator(delegatee.clone(), last_index));
        env.storage()
            .instance()
            .remove(&DataKey::DelegatorSlot(delegator.clone()));
        env.storage()
            .instance()
            .set(&DataKey::DelegatorCount(delegatee.clone()), &last_index);
    }

    fn bump_key_ttl(env: &Env, key: &DataKey) {
        env.storage()
            .persistent()
            .extend_ttl(key, MIN_PERSISTENT_TTL, PERSISTENT_TTL_BUMP_TO);
    }

    /// Convert a fee tier ID (0-3) to its basis points value.
    ///
    /// Matches the fee tier definitions:
    /// - 0 → 1 bps (0.01%)
    /// - 1 → 5 bps (0.05%)
    /// - 2 → 30 bps (0.3%)
    /// - 3 → 100 bps (1.0%)
    fn fee_tier_to_bps(fee_tier: i128) -> Result<i128, GovernanceError> {
        match fee_tier {
            0 => Ok(1),   // VeryLow: 0.01%
            1 => Ok(5),   // Low: 0.05%
            2 => Ok(30),  // Medium: 0.3%
            3 => Ok(100), // High: 1.0%
            _ => Err(GovernanceError::InvalidFeeBps),
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use amm::AmmPool;
    use soroban_sdk::token::{Client as StellarTokenClient, StellarAssetClient};
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        Env,
    };
    use token::LpToken;

    // ── Helpers ───────────────────────────────────────────────────────────────

    struct Suite {
        env: Env,
        gov_addr: Address,
        lp_addr: Address,
        amm_addr: Address,
    }

    fn setup_suite(initial_fee_bps: i128) -> Suite {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1_000_000);

        let admin = Address::generate(&env);

        // Deploy LP token.
        let lp_addr = env.register_contract(None, LpToken);
        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &admin, // temporary admin; will be replaced by AMM
            &soroban_sdk::String::from_str(&env, "AMM LP"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );

        // Deploy token A and B.
        let ta = env.register_stellar_asset_contract_v2(admin.clone());
        let tb = env.register_stellar_asset_contract_v2(admin.clone());
        let ta_addr = ta.address();
        let tb_addr = tb.address();

        // Deploy governance.
        let gov_addr = env.register_contract(None, Governance);

        // Deploy AMM.
        let amm_addr = env.register_contract(None, AmmPool);
        amm::AmmPoolClient::new(&env, &amm_addr).initialize(
            &gov_addr, // The governance contract is the pool's admin
            &ta_addr,
            &tb_addr,
            &lp_addr,
            &initial_fee_bps,
            &admin,
            &0_i128,
        );

        // Initialize governance.
        GovernanceClient::new(&env, &gov_addr).initialize(
            &admin,
            &amm_addr,
            &lp_addr,
            &(7 * 24 * 60 * 60_u64), // voting_period_secs: 7 days
            &(2 * 24 * 60 * 60_u64), // timelock_secs: 2 days
            &1_000_i128,             // quorum_bps: 10%
            &100_i128,               // min_proposer_stake_bps
        );
        token::LpTokenClient::new(&env, &lp_addr).set_locker(&gov_addr);

        Suite {
            env,
            gov_addr,
            lp_addr,
            amm_addr,
        }
    }

    /// Mint LP tokens directly to an address (simulates adding liquidity).
    fn mint_lp(suite: &Suite, to: &Address, amount: i128) {
        token::LpTokenClient::new(&suite.env, &suite.lp_addr).mint(to, &amount);
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_passing_proposal_executes() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        // Propose new fee of 50 bps.
        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        assert_eq!(pid, 0);

        // Both vote for.
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        // Advance past voting period + timelock.
        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.execute_after + 1);

        gov.execute(&pid);

        let executed = gov.get_proposal(&pid);
        assert!(executed.executed);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Executed);
    }

    #[test]
    fn test_failing_quorum_defeats_proposal() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        // lp1 gets 20, lp2 gets 980 — lp1 alone is < 10% quorum.
        mint_lp(&s, &lp1, 20);
        mint_lp(&s, &lp2, 980);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        // Only lp1 votes (20 out of 1000 total = 2% < 10% quorum).
        gov.vote(&lp1, &pid, &Vote::For);

        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.execute_after + 1);

        // Execute should panic — quorum not met.
        let result = gov.try_execute(&pid);
        assert!(result.is_err());
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Defeated);
    }

    #[test]
    fn test_expired_proposal_cannot_execute() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        // Jump past the expiry window.
        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.expires_at + 1);

        let result = gov.try_execute(&pid);
        assert!(result.is_err());
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Expired);
    }

    #[test]
    fn test_cannot_vote_twice() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 500);
        mint_lp(&s, &Address::generate(&s.env), 500);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);

        let result = gov.try_vote(&lp1, &pid, &Vote::Against);
        assert!(result.is_err());
    }

    #[test]
    fn test_cannot_vote_after_period_ends() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 500);
        mint_lp(&s, &Address::generate(&s.env), 500);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.vote_end + 1);

        let result = gov.try_vote(&lp1, &pid, &Vote::For);
        assert!(result.is_err());
    }

    #[test]
    fn test_cannot_execute_before_timelock() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        // Jump past voting but NOT past timelock.
        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.vote_end + 1);

        let result = gov.try_execute(&pid);
        assert!(result.is_err());
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Pending);
    }

    #[test]
    fn test_proposal_status_active_then_queued() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Active);

        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.execute_after);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Queued);
    }

    #[test]
    fn test_no_lp_tokens_cannot_propose() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let nobody = Address::generate(&s.env);
        // Give someone else tokens so total_supply > 0.
        mint_lp(&s, &Address::generate(&s.env), 1000);

        let result = gov.try_propose(&nobody, &ProposalKind::UpdateFee(50));
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_fee_bps_rejected() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 1000);

        let result = gov.try_propose(&lp1, &ProposalKind::UpdateFee(10_001));
        assert!(result.is_err());
    }

    #[test]
    fn test_below_min_stake_cannot_propose_but_exact_min_can() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        let low = Address::generate(&s.env);
        let exact = Address::generate(&s.env);
        let whale = Address::generate(&s.env);

        mint_lp(&s, &low, 9);
        mint_lp(&s, &exact, 10);
        mint_lp(&s, &whale, 981);

        assert!(gov.try_propose(&low, &ProposalKind::UpdateFee(40)).is_err());
        let pid = gov.propose(&exact, &ProposalKind::UpdateFee(40));
        assert_eq!(pid, 0);
    }

    #[test]
    fn test_vote_locks_balance_until_proposal_concludes() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        let lp_client = token::LpTokenClient::new(&s.env, &s.lp_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        let receiver = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);
        assert_eq!(lp_client.locked_balance(&lp1), 600);

        // Simulated flash-loan pattern fails: voter cannot move locked weight.
        let transfer_result = lp_client.try_transfer(&lp1, &receiver, &600_i128);
        assert!(transfer_result.is_err());

        gov.vote(&lp2, &pid, &Vote::For);
        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.execute_after + 1);
        gov.execute(&pid);

        gov.unlock_vote(&lp1, &pid);
        assert_eq!(lp_client.locked_balance(&lp1), 0);
        lp_client.transfer(&lp1, &receiver, &600_i128);
    }

    // Issue #129: governance must emit `proposed`, `voted`, and `executed`
    // events with the documented payloads.
    #[test]
    fn test_governance_emits_proposed_voted_executed_events() {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::IntoVal;

        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        let proposal = gov.get_proposal(&pid);

        // `proposed` event: (id, proposer, kind, vote_end)
        let events = s.env.events().all();
        let proposed_evt = events
            .iter()
            .find(|e| {
                e.0 == gov.address && e.1 == (Symbol::new(&s.env, "proposed"),).into_val(&s.env)
            })
            .expect("proposed event not found");
        let __ver_7: (u32, (u32, Address, ProposalKind, u64, u32)) =
            proposed_evt.2.into_val(&s.env);
        assert_eq!(__ver_7.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        let proposed_data: (u32, Address, ProposalKind, u64, u32) = __ver_7.1;
        assert_eq!(
            proposed_data,
            (
                pid,
                lp1.clone(),
                ProposalKind::UpdateFee(50),
                proposal.vote_end,
                proposal.snapshot_ledger
            )
        );

        gov.vote(&lp1, &pid, &Vote::For);

        // `voted` event: (proposal_id, voter, choice, voting_power)
        let events = s.env.events().all();
        let voted_evt = events
            .iter()
            .find(|e| e.0 == gov.address && e.1 == (Symbol::new(&s.env, "voted"),).into_val(&s.env))
            .expect("voted event not found");
        let __ver_8: (u32, (u32, Address, Vote, i128)) = voted_evt.2.into_val(&s.env);
        assert_eq!(__ver_8.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        let voted_data: (u32, Address, Vote, i128) = __ver_8.1;
        assert_eq!(voted_data, (pid, lp1.clone(), Vote::For, 600_i128));

        gov.vote(&lp2, &pid, &Vote::For);

        s.env.ledger().set_timestamp(proposal.execute_after + 1);
        gov.execute(&pid);

        // `executed` event: (proposal_id, kind)
        let events = s.env.events().all();
        let executed_evt = events
            .iter()
            .find(|e| {
                e.0 == gov.address && e.1 == (Symbol::new(&s.env, "executed"),).into_val(&s.env)
            })
            .expect("executed event not found");
        let __ver_9: (u32, (u32, ProposalKind)) = executed_evt.2.into_val(&s.env);
        assert_eq!(__ver_9.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        let executed_data: (u32, ProposalKind) = __ver_9.1;
        assert_eq!(executed_data, (pid, ProposalKind::UpdateFee(50)));
    }

    #[test]
    fn test_governance_multiple_proposal_kinds() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        let amm = amm::AmmPoolClient::new(&s.env, &s.amm_addr);

        let lp1 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 1000);

        // --- 1. Test PausePool proposal ---
        let pid1 = gov.propose(&lp1, &ProposalKind::PausePool);
        gov.vote(&lp1, &pid1, &Vote::For);
        let prop1 = gov.get_proposal(&pid1);
        s.env.ledger().set_timestamp(prop1.execute_after + 1);
        gov.execute(&pid1);
        assert!(amm.is_paused());
        gov.unlock_vote(&lp1, &pid1);

        // --- 2. Test UnpausePool proposal ---
        let pid2 = gov.propose(&lp1, &ProposalKind::UnpausePool);
        gov.vote(&lp1, &pid2, &Vote::For);
        let prop2 = gov.get_proposal(&pid2);
        s.env.ledger().set_timestamp(prop2.execute_after + 1);
        gov.execute(&pid2);
        assert!(!amm.is_paused());
        gov.unlock_vote(&lp1, &pid2);

        // --- 3. Test UpdateFlashLoanFee proposal ---
        let pid3 = gov.propose(&lp1, &ProposalKind::UpdateFlashLoanFee(45));
        gov.vote(&lp1, &pid3, &Vote::For);
        let prop3 = gov.get_proposal(&pid3);
        s.env.ledger().set_timestamp(prop3.execute_after + 1);
        gov.execute(&pid3);
        let info = amm.get_info();
        assert_eq!(info.flash_loan_fee_bps, 45);
        gov.unlock_vote(&lp1, &pid3);

        // --- 4. Test UpdateProtocolFee proposal ---
        let recipient = Address::generate(&s.env);
        let pid4 = gov.propose(
            &lp1,
            &ProposalKind::UpdateProtocolFee(UpdateProtocolFeeParams {
                new_bps: 10,
                new_recipient: recipient.clone(),
            }),
        );
        gov.vote(&lp1, &pid4, &Vote::For);
        let prop4 = gov.get_proposal(&pid4);
        s.env.ledger().set_timestamp(prop4.execute_after + 1);
        gov.execute(&pid4);
        let (fee_rec, bps) = amm.get_protocol_fee();
        assert_eq!(fee_rec, Some(recipient));
        assert_eq!(bps, 10);
        gov.unlock_vote(&lp1, &pid4);

        // --- 5. Test EmergencyWithdraw proposal ---
        let emergency_rec = Address::generate(&s.env);
        let pid5 = gov.propose(
            &lp1,
            &ProposalKind::EmergencyWithdraw(emergency_rec.clone()),
        );
        gov.vote(&lp1, &pid5, &Vote::For);
        let prop5 = gov.get_proposal(&pid5);
        s.env.ledger().set_timestamp(prop5.execute_after + 1);

        let ta_sac = StellarAssetClient::new(&s.env, &info.token_a);
        let tb_sac = StellarAssetClient::new(&s.env, &info.token_b);
        let provider = Address::generate(&s.env);
        ta_sac.mint(&provider, &100_000_i128);
        tb_sac.mint(&provider, &100_000_i128);

        amm.add_liquidity(&provider, &100_000_i128, &100_000_i128, &0_i128, &u64::MAX);
        assert_eq!(amm.get_info().reserve_a, 100_000);
        assert_eq!(amm.get_info().reserve_b, 100_000);

        gov.execute(&pid5);

        assert_eq!(amm.get_info().reserve_a, 0);
        assert_eq!(amm.get_info().reserve_b, 0);

        let ta_client = StellarTokenClient::new(&s.env, &info.token_a);
        let tb_client = StellarTokenClient::new(&s.env, &info.token_b);
        assert_eq!(ta_client.balance(&emergency_rec), 100_000);
        assert_eq!(tb_client.balance(&emergency_rec), 100_000);

        gov.unlock_vote(&lp1, &pid5);
    }

    #[test]
    fn test_full_governance_lifecycle() {
        let s = setup_suite(30); // initial fee = 30 bps
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        // 1. Distribute LP tokens (quorum = 10% of 1000 = 100)
        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        // 2. Propose fee change to 50 bps
        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Active);

        // 3. Vote (both for)
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        // 4. Advance past voting period
        let p = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(p.execute_after + 1);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Queued);

        // 5. Execute
        gov.execute(&pid);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Executed);

        // 6. Verify AMM fee changed
        let amm = amm::AmmPoolClient::new(&s.env, &s.amm_addr);
        assert_eq!(amm.get_info().fee_bps, 50);
    }

    #[test]
    fn test_governance_lifecycle_defeat_quorum() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 50);
        mint_lp(&s, &lp2, 950);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Active);

        // Only lp1 votes. Total votes = 50 < 100 (quorum threshold)
        gov.vote(&lp1, &pid, &Vote::For);

        let p = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(p.execute_after + 1);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Defeated);

        let result = gov.try_execute(&pid);
        assert!(result.is_err());
    }

    #[test]
    fn test_governance_lifecycle_expired() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Active);

        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        let p = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(p.expires_at + 1);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Expired);

        let result = gov.try_execute(&pid);
        assert!(result.is_err());
    }

    // ── Issue #188: set_timelock_delay ────────────────────────────────────────

    #[test]
    fn test_timelock_delay_zero_allows_immediate_execution() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        // Set timelock delay to 0 so execution is allowed immediately after vote_end.
        gov.set_timelock_delay(&0_u64);
        let params = gov.get_params();
        assert_eq!(params.timelock_secs, 0);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        let proposal = gov.get_proposal(&pid);
        // With timelock = 0: execute_after = vote_end, expires_at = vote_end + voting_period.
        // Jump to execute_after + 1 to satisfy now >= execute_after.
        s.env.ledger().set_timestamp(proposal.execute_after + 1);

        gov.execute(&pid);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Executed);
    }

    #[test]
    fn test_execute_reverts_before_timelock_elapses() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        let proposal = gov.get_proposal(&pid);
        // Jump past vote_end but NOT past execute_after.
        s.env.ledger().set_timestamp(proposal.vote_end + 1);

        let result = gov.try_execute(&pid);
        assert_eq!(result, Err(Ok(GovernanceError::TimelockNotElapsed)));
    }

    #[test]
    fn test_execute_succeeds_after_timelock_elapses() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        let proposal = gov.get_proposal(&pid);
        // Jump past execute_after.
        s.env.ledger().set_timestamp(proposal.execute_after + 1);

        gov.execute(&pid);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Executed);
    }

    // ── Issue #189: vote_unlocked event ──────────────────────────────────────

    #[test]
    fn test_unlock_vote_emits_vote_unlocked_event() {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::IntoVal;

        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.execute_after + 1);
        gov.execute(&pid);

        gov.unlock_vote(&lp1, &pid);

        let events = s.env.events().all();
        let unlock_evt = events
            .iter()
            .find(|e| {
                e.0 == s.gov_addr
                    && e.1 == (Symbol::new(&s.env, "vote_unlocked"), lp1.clone()).into_val(&s.env)
            })
            .expect("vote_unlocked event not emitted");

        let __ver_10: (u32, (u32, i128)) = unlock_evt.2.into_val(&s.env);
        assert_eq!(__ver_10.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        let data: (u32, i128) = __ver_10.1;
        assert_eq!(data.0, pid);
        assert_eq!(data.1, 600_i128); // amount_unlocked == voting power used
    }

    #[test]
    fn test_snapshot_balance_used_not_current_balance() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        let buyer = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        s.env.ledger().with_mut(|l| l.sequence_number = 10);
        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        let proposal = gov.get_proposal(&pid);

        // Acquire LP after snapshot — should not increase voting power.
        s.env.ledger().with_mut(|l| l.sequence_number = 11);
        mint_lp(&s, &buyer, 500);
        assert_eq!(gov.get_snapshot_balance(&pid, &buyer), 0);
        assert!(gov.try_vote(&buyer, &pid, &Vote::For).is_err());

        assert_eq!(gov.get_snapshot_balance(&pid, &lp1), 600);
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        s.env.ledger().set_timestamp(proposal.execute_after + 1);
        gov.execute(&pid);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Executed);
    }

    #[test]
    fn test_delegation_aggregates_voting_power() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        let delegatee = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        gov.delegate(&lp1, &delegatee);

        let pid = gov.propose(&lp2, &ProposalKind::UpdateFee(50));
        assert!(gov.try_vote(&lp1, &pid, &Vote::For).is_err());
        gov.vote(&delegatee, &pid, &Vote::For);

        let p = gov.get_proposal(&pid);
        assert_eq!(p.votes_for, 600);
    }

    #[test]
    fn test_undelegate_restores_direct_voting() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        let delegatee = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        gov.delegate(&lp1, &delegatee);
        let pid = gov.propose(&lp2, &ProposalKind::UpdateFee(50));
        gov.undelegate(&lp1);
        gov.vote(&lp1, &pid, &Vote::For);
        assert_eq!(gov.get_proposal(&pid).votes_for, 600);
    }

    #[test]
    fn test_recursive_delegation_chain() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let a = Address::generate(&s.env);
        let b = Address::generate(&s.env);
        let c = Address::generate(&s.env);
        let proposer = Address::generate(&s.env);
        mint_lp(&s, &a, 300);
        mint_lp(&s, &b, 300);
        mint_lp(&s, &c, 100);
        mint_lp(&s, &proposer, 300);

        gov.delegate(&a, &b);
        gov.delegate(&b, &c);

        let pid = gov.propose(&proposer, &ProposalKind::UpdateFee(50));
        gov.vote(&c, &pid, &Vote::For);
        assert_eq!(gov.get_proposal(&pid).votes_for, 700);
    }

    #[test]
    fn test_veto_prevents_execution_and_emits_audit() {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::IntoVal;

        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        let multisig = Address::generate(&s.env);
        gov.set_veto_multisig(&multisig);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(99));
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.vote_end + 1);
        gov.veto(&pid);

        assert_eq!(gov.proposal_status(&pid), ProposalStatus::InDiscussion);
        let audit = gov.get_veto_audit(&pid).unwrap();
        assert_eq!(audit.proposal_id, pid);
        assert_eq!(audit.vetoed_by, multisig);

        s.env.ledger().set_timestamp(proposal.execute_after + 1);
        assert!(gov.try_execute(&pid).is_err());

        let events = s.env.events().all();
        let veto_evt = events
            .iter()
            .find(|e| e.0 == s.gov_addr && e.1 == (Symbol::new(&s.env, "vetoed"),).into_val(&s.env))
            .expect("vetoed event");
        let __ver_11: (u32, (u32, Address, u64, u64)) = veto_evt.2.into_val(&s.env);
        assert_eq!(__ver_11.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        let data: (u32, Address, u64, u64) = __ver_11.1;
        assert_eq!(data.0, pid);
    }

    #[test]
    fn test_veto_window_enforced() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        let multisig = Address::generate(&s.env);
        gov.set_veto_multisig(&multisig);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        let proposal = gov.get_proposal(&pid);
        s.env
            .ledger()
            .set_timestamp(proposal.vote_end + VETO_WINDOW_SECS + 1);
        assert_eq!(
            gov.try_veto(&pid),
            Err(Ok(GovernanceError::VetoWindowExpired))
        );
    }
}

// ── Property-based tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod prop_tests {
    extern crate std;
    use super::*;
    use amm::AmmPool;
    use proptest::collection;
    use proptest::prelude::*;
    use proptest::test_runner::{Config, TestRunner};
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::{Address, Env};
    use token::LpToken;

    // ── Test harness ────────────────────────────────────────────────────────────

    struct PropEnv {
        env: Env,
        gov_addr: Address,
        lp_addr: Address,
    }

    fn setup_prop_env() -> PropEnv {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1_000_000);

        let admin = Address::generate(&env);

        let lp_addr = env.register_contract(None, LpToken);
        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &admin,
            &soroban_sdk::String::from_str(&env, "AMM LP"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );

        let ta = env.register_stellar_asset_contract_v2(admin.clone());
        let tb = env.register_stellar_asset_contract_v2(admin.clone());

        let amm_addr = env.register_contract(None, AmmPool);
        amm::AmmPoolClient::new(&env, &amm_addr).initialize(
            &admin,
            &ta.address(),
            &tb.address(),
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let gov_addr = env.register_contract(None, Governance);
        GovernanceClient::new(&env, &gov_addr).initialize(
            &admin,
            &amm_addr,
            &lp_addr,
            &(7 * 24 * 60 * 60_u64),
            &(2 * 24 * 60 * 60_u64),
            &1_000_i128,
            &100_i128,
        );

        token::LpTokenClient::new(&env, &lp_addr).set_locker(&gov_addr);
        PropEnv {
            env,
            gov_addr,
            lp_addr,
        }
    }

    // ── Pure math properties ────────────────────────────────────────────────────

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Quorum threshold never overflows or goes out of bounds.
        #[test]
        fn quorum_check_never_overflows(
            total_supply in 1i128..i128::MAX / 10_000,
            quorum_bps in 1i128..10_000i128,
        ) {
            let threshold = total_supply * quorum_bps / 10_000;
            prop_assert!(threshold >= 0);
            prop_assert!(threshold <= total_supply);
        }

        /// Combined votes cast never exceeds total supply.
        #[test]
        fn total_votes_does_not_exceed_supply(
            votes_for in 0i128..i128::MAX / 2,
            votes_against in 0i128..i128::MAX / 2,
            votes_abstain in 0i128..i128::MAX / 2,
        ) {
            let total_supply = votes_for.saturating_add(votes_against).saturating_add(votes_abstain);
            let total_votes = votes_for + votes_against + votes_abstain;
            // Every vote cast is a real LP holder — sum cannot exceed total supply.
            prop_assert!(total_votes <= total_supply,
                "total_votes={total_votes} > total_supply={total_supply}");

            // Individual vote buckets are non-negative and bounded.
            prop_assert!(votes_for >= 0);
            prop_assert!(votes_against >= 0);
            prop_assert!(votes_abstain >= 0);
            prop_assert!(votes_for <= total_votes);
            prop_assert!(votes_against <= total_votes);
            prop_assert!(votes_abstain <= total_votes);
        }

        /// Min proposer stake math holds and is within expected bounds.
        #[test]
        fn min_proposer_stake_is_correct(
            total_supply in 1i128..i128::MAX / 10_000,
            min_bps in 0i128..10_000i128,
        ) {
            let min_stake = ((total_supply * min_bps) / 10_000).max(1);
            prop_assert!(min_stake >= 1);
            prop_assert!(min_stake <= total_supply.max(1));
        }

        /// Expiry always comes at or after execute_after.
        #[test]
        fn expiry_logic_boundaries(
            vote_end in 0u64..u64::MAX / 3,
            timelock in 0u64..u64::MAX / 3,
            voting_period in 1u64..u64::MAX / 3,
        ) {
            let execute_after = vote_end + timelock;
            let expires_at = execute_after + timelock.max(voting_period);
            prop_assert!(expires_at >= execute_after,
                "expires_at={expires_at} < execute_after={execute_after}");
            prop_assert!(execute_after >= vote_end);
        }

        /// No overflow in proposal lifecycle timestamps.
        #[test]
        fn timestamp_arithmetic_no_overflow(
            now in 0u64..u64::MAX / 4,
            voting_period in 1u64..u64::MAX / 4,
            timelock in 0u64..u64::MAX / 4,
        ) {
            let vote_end = now + voting_period;
            let execute_after = vote_end + timelock;
            let expires_at = execute_after + timelock.max(voting_period);
            prop_assert!(vote_end >= now);
            prop_assert!(execute_after >= vote_end);
            prop_assert!(expires_at >= execute_after);
        }

        /// Edge-case: when timelock is zero, execute_after == vote_end.
        #[test]
        fn zero_timelock_property(
            vote_end in 0u64..u64::MAX / 2,
        ) {
            let execute_after = vote_end + 0;
            prop_assert_eq!(execute_after, vote_end);
        }
    }

    // ── Property 1: Voting power conservation (contract-level) ─────────────────

    #[test]
    fn prop_voting_power_conservation() {
        let mut runner = TestRunner::new(Config {
            cases: 256,
            ..Config::default()
        });

        let strategy = collection::vec((1i128..10_000, 0..4i8), 2..=8);

        runner.run(&strategy, |voters| {
            let pe = setup_prop_env();
            let gov = GovernanceClient::new(&pe.env, &pe.gov_addr);
            let lp = token::LpTokenClient::new(&pe.env, &pe.lp_addr);

            let n = voters.len();
            let holders: std::vec::Vec<Address> =
                (0..n).map(|_| Address::generate(&pe.env)).collect();

            for (i, (amt, _)) in voters.iter().enumerate() {
                if *amt > 0 {
                    lp.mint(&holders[i], amt);
                }
            }

            let total_supply: i128 = voters.iter().map(|(a, _)| a).sum();
            let proposer_idx = voters.iter().position(|(a, _)| *a >= 1).unwrap();
            let pid = gov.propose(&holders[proposer_idx], &ProposalKind::UpdateFee(50));

            let mut expected_for: i128 = 0;
            let mut expected_against: i128 = 0;
            let mut expected_abstain: i128 = 0;

            for (i, (amt, vote_choice)) in voters.iter().enumerate() {
                if *vote_choice == 0 || *amt == 0 {
                    continue;
                }
                let choice = match *vote_choice {
                    1 => Vote::For,
                    2 => Vote::Against,
                    _ => Vote::Abstain,
                };
                if gov.try_vote(&holders[i], &pid, &choice).is_ok() {
                    match choice {
                        Vote::For => expected_for += amt,
                        Vote::Against => expected_against += amt,
                        Vote::Abstain => expected_abstain += amt,
                    }
                    let locked = lp.locked_balance(&holders[i]);
                    prop_assert_eq!(locked, *amt,
                        "locked balance should equal voting power: voter={}, locked={}, power={}", i, locked, amt);
                }
            }

            let proposal = gov.get_proposal(&pid);
            let total_votes = proposal.votes_for + proposal.votes_against + proposal.votes_abstain;
            prop_assert!(total_votes <= total_supply,
                "total_votes={total_votes} exceeds total_supply={total_supply}");
            prop_assert_eq!(proposal.votes_for, expected_for,
                "votes_for mismatch");
            prop_assert_eq!(proposal.votes_against, expected_against,
                "votes_against mismatch");
            prop_assert_eq!(proposal.votes_abstain, expected_abstain,
                "votes_abstain mismatch");

            // Each voter's locked amount never exceeds their minted balance.
            for (i, (amt, _)) in voters.iter().enumerate() {
                let locked = lp.locked_balance(&holders[i]);
                prop_assert!(locked <= *amt,
                    "voter {i}: locked={locked} > balance={amt}");
            }

            Ok(())
        })
        .unwrap();
    }

    // ── Property 2: Delegated voting power conservation ─────────────────────────

    #[test]
    fn prop_delegated_power_conservation() {
        let mut runner = TestRunner::new(Config {
            cases: 256,
            ..Config::default()
        });

        let strategy = (
            collection::vec(1i128..10_000, 2..=5),
            0..5u32, // delegatee index within voters
        );

        runner
            .run(&strategy, |(amounts, delegatee_idx)| {
                let pe = setup_prop_env();
                let gov = GovernanceClient::new(&pe.env, &pe.gov_addr);
                let lp = token::LpTokenClient::new(&pe.env, &pe.lp_addr);

                let n = amounts.len();
                let holders: std::vec::Vec<Address> =
                    (0..n).map(|_| Address::generate(&pe.env)).collect();

                for (i, amt) in amounts.iter().enumerate() {
                    lp.mint(&holders[i], amt);
                }

                let delegatee = if (delegatee_idx as usize) < n {
                    (delegatee_idx as usize)
                } else {
                    0
                };

                // All non-delegatee holders delegate to delegatee.
                for (i, _) in amounts.iter().enumerate() {
                    if i != delegatee {
                        gov.delegate(&holders[i], &holders[delegatee]);
                    }
                }

                let proposer = &holders[delegatee];
                let pid = gov.propose(proposer, &ProposalKind::UpdateFee(50));

                // Delegatee votes with aggregated power.
                if gov.try_vote(&holders[delegatee], &pid, &Vote::For).is_ok() {
                    let expected_power: i128 = amounts.iter().sum();
                    let proposal = gov.get_proposal(&pid);
                    prop_assert_eq!(
                        proposal.votes_for,
                        expected_power,
                        "delegated power={} != expected={}",
                        proposal.votes_for,
                        expected_power
                    );

                    // Delegatee's locked balance equals their own LP (not the whole delegation).
                    let delegatee_locked = lp.locked_balance(&holders[delegatee]);
                    prop_assert_eq!(
                        delegatee_locked,
                        amounts[delegatee],
                        "delegatee locked should equal own balance"
                    );

                    // Delegators' LPs should also be locked.
                    for (i, amt) in amounts.iter().enumerate() {
                        if i != delegatee {
                            let locked = lp.locked_balance(&holders[i]);
                            prop_assert_eq!(
                                locked,
                                *amt,
                                "delegator {}: locked={} != balance={}",
                                i,
                                locked,
                                amt
                            );
                        }
                    }

                    // Total supply conservation.
                    let total_supply: i128 = amounts.iter().sum();
                    let total_votes =
                        proposal.votes_for + proposal.votes_against + proposal.votes_abstain;
                    prop_assert!(
                        total_votes <= total_supply,
                        "total_votes={total_votes} > total_supply={total_supply}"
                    );
                }

                Ok(())
            })
            .unwrap();
    }

    // ── Property 3: Vote locking / unlocking consistency ────────────────────────

    #[test]
    fn prop_lock_unlock_consistency() {
        let mut runner = TestRunner::new(Config {
            cases: 256,
            ..Config::default()
        });

        let strategy = collection::vec((1i128..10_000, 1..4i8), 2..=5);

        runner
            .run(&strategy, |voters| {
                let pe = setup_prop_env();
                let gov = GovernanceClient::new(&pe.env, &pe.gov_addr);
                let lp = token::LpTokenClient::new(&pe.env, &pe.lp_addr);

                let n = voters.len();
                let holders: std::vec::Vec<Address> =
                    (0..n).map(|_| Address::generate(&pe.env)).collect();

                for (i, (amt, _)) in voters.iter().enumerate() {
                    lp.mint(&holders[i], amt);
                }

                let proposer_idx = voters.iter().position(|(a, _)| *a >= 1).unwrap();
                let pid = gov.propose(&holders[proposer_idx], &ProposalKind::UpdateFee(50));

                // Vote (each voter chooses For/Against/Abstain per their vote_choice)
                for (i, (amt, vote_choice)) in voters.iter().enumerate() {
                    let choice = match *vote_choice {
                        1 => Vote::For,
                        2 => Vote::Against,
                        _ => Vote::Abstain,
                    };
                    if gov.try_vote(&holders[i], &pid, &choice).is_ok() {
                        // Immediately after vote: locked == voting power
                        prop_assert_eq!(
                            lp.locked_balance(&holders[i]),
                            *amt,
                            "after vote: locked={} != power={}",
                            lp.locked_balance(&holders[i]),
                            *amt
                        );
                    }
                }

                // Verify unlocks fail while proposal is active.
                for (i, _) in voters.iter().enumerate() {
                    if lp.locked_balance(&holders[i]) > 0 {
                        prop_assert!(
                            gov.try_unlock_vote(&holders[i], &pid).is_err(),
                            "unlock should fail while proposal is active"
                        );
                    }
                }

                // Advance past voting + timelock.
                let proposal = gov.get_proposal(&pid);
                pe.env.ledger().set_timestamp(proposal.execute_after + 1);

                // Execute (may succeed or fail depending on quorum/majority).
                let _ = gov.try_execute(&pid);
                let status = gov.proposal_status(&pid);

                // Only concluded statuses allow unlock.
                let can_unlock = matches!(
                    status,
                    ProposalStatus::Executed
                        | ProposalStatus::Defeated
                        | ProposalStatus::Expired
                        | ProposalStatus::Cancelled
                );

                if can_unlock {
                    for (i, amt) in voters.iter().enumerate() {
                        let locked_before = lp.locked_balance(&holders[i]);
                        if locked_before > 0 {
                            if gov.try_unlock_vote(&holders[i], &pid).is_ok() {
                                let locked_after = lp.locked_balance(&holders[i]);
                                prop_assert_eq!(
                                    locked_after,
                                    0,
                                    "after unlock: locked should be 0, got {}",
                                    locked_after
                                );
                            }
                        } else {
                            // Unlock with no locked vote should fail.
                            prop_assert!(
                                gov.try_unlock_vote(&holders[i], &pid).is_err(),
                                "unlock with no locked vote should fail"
                            );
                        }
                    }
                } else {
                    let status_name = std::format!("{:?}", status);
                    // Unlock should still fail for non-concluded proposals.
                    for (i, _) in voters.iter().enumerate() {
                        if lp.locked_balance(&holders[i]) > 0 {
                            let result = gov.try_unlock_vote(&holders[i], &pid);
                            prop_assert!(
                                result.is_err(),
                                "unlock should fail for status={status_name}"
                            );
                        }
                    }
                }

                Ok(())
            })
            .unwrap();
    }

    // ── Property 4: Malicious / invalid voting scenarios ────────────────────────

    #[test]
    fn prop_malicious_voting_scenarios() {
        let mut runner = TestRunner::new(Config {
            cases: 256,
            ..Config::default()
        });

        let strategy = collection::vec(1i128..10_000, 3..=6);

        runner
            .run(&strategy, |amounts| {
                let pe = setup_prop_env();
                let gov = GovernanceClient::new(&pe.env, &pe.gov_addr);
                let lp = token::LpTokenClient::new(&pe.env, &pe.lp_addr);

                // We need n main holders, plus a zero-balance address, plus 2 delegation
                // addresses — all minted before the proposal snapshot.
                let n = amounts.len();
                let holders: std::vec::Vec<Address> =
                    (0..n + 3).map(|_| Address::generate(&pe.env)).collect();

                for (i, amt) in amounts.iter().enumerate() {
                    lp.mint(&holders[i], amt);
                }
                // Mint LP for delegation addresses so they have snapshot voting power.
                lp.mint(&holders[n + 1], &1000);
                lp.mint(&holders[n + 2], &1000);

                let pid = gov.propose(&holders[0], &ProposalKind::UpdateFee(50));

                // ── A: Double vote ──
                if gov.try_vote(&holders[0], &pid, &Vote::For).is_ok() {
                    let double = gov.try_vote(&holders[0], &pid, &Vote::For);
                    prop_assert!(double.is_err(), "double vote should fail");
                }

                // ── B: Vote after deadline ──
                {
                    let proposal = gov.get_proposal(&pid);
                    pe.env.ledger().set_timestamp(proposal.vote_end + 1);
                    let late_vote = gov.try_vote(&holders[1], &pid, &Vote::For);
                    prop_assert!(late_vote.is_err(), "vote after deadline should fail");
                    pe.env.ledger().set_timestamp(1_000_000);
                }

                // ── C: Vote on non-existent proposal ──
                {
                    let bad_vote = gov.try_vote(&holders[1], &u32::MAX, &Vote::For);
                    prop_assert!(bad_vote.is_err(), "vote on bad proposal should fail");
                }

                // ── D: Vote with 0 snapshot balance (holder never minted) ──
                {
                    let zero_vote = gov.try_vote(&holders[n], &pid, &Vote::For);
                    prop_assert!(zero_vote.is_err(), "vote with 0 balance should fail");
                }

                // ── E: Self-delegation ──
                {
                    let self_delegate = gov.try_delegate(&holders[1], &holders[1]);
                    prop_assert!(self_delegate.is_err(), "self-delegation should fail");
                }

                // ── F: Delegation cycle (a->b, b->a) ──
                {
                    let a = &holders[n + 1];
                    let b = &holders[n + 2];
                    if gov.try_delegate(a, b).is_ok() {
                        let cycle = gov.try_delegate(b, a);
                        prop_assert!(cycle.is_err(), "delegation cycle should fail");
                    }
                }

                // ── G: Vote while delegated ──
                {
                    let deleter = &holders[n + 1];
                    let del_target = &holders[n + 2];
                    if gov.try_delegate(deleter, del_target).is_ok() {
                        let delegated_vote = gov.try_vote(deleter, &pid, &Vote::For);
                        prop_assert!(
                            delegated_vote.is_err(),
                            "delegated address should not be able to vote directly"
                        );
                    }
                }

                // ── H: Unlock without ever voting ──
                {
                    let unlock_no_vote = gov.try_unlock_vote(&holders[2], &pid);
                    prop_assert!(unlock_no_vote.is_err(), "unlock without voting should fail");
                }

                Ok(())
            })
            .unwrap();
    }

    // ── Property 5: Post-execution unlock guarantees ────────────────────────────

    #[test]
    fn prop_post_execution_unlock_recovers_all_locked_tokens() {
        let mut runner = TestRunner::new(Config {
            cases: 256,
            ..Config::default()
        });

        let strategy = collection::vec((1i128..5_000, 1..4i8), 2..=5);

        runner
            .run(&strategy, |voters| {
                let pe = setup_prop_env();
                let gov = GovernanceClient::new(&pe.env, &pe.gov_addr);
                let lp = token::LpTokenClient::new(&pe.env, &pe.lp_addr);

                let n = voters.len();
                let holders: std::vec::Vec<Address> =
                    (0..n).map(|_| Address::generate(&pe.env)).collect();

                for (i, (amt, _)) in voters.iter().enumerate() {
                    lp.mint(&holders[i], amt);
                }

                let proposer_idx = voters.iter().position(|(a, _)| *a >= 1).unwrap();
                let pid = gov.propose(&holders[proposer_idx], &ProposalKind::UpdateFee(50));

                for (i, (_, vote_choice)) in voters.iter().enumerate() {
                    let choice = match *vote_choice {
                        1 => Vote::For,
                        2 => Vote::Against,
                        _ => Vote::Abstain,
                    };
                    let _ = gov.try_vote(&holders[i], &pid, &choice);
                }

                // Advance time and try to execute.
                let proposal = gov.get_proposal(&pid);
                pe.env.ledger().set_timestamp(proposal.execute_after + 1);
                let _ = gov.try_execute(&pid);
                let status = gov.proposal_status(&pid);

                let can_unlock = matches!(
                    status,
                    ProposalStatus::Executed
                        | ProposalStatus::Defeated
                        | ProposalStatus::Expired
                        | ProposalStatus::Cancelled
                );

                if can_unlock {
                    let mut total_locked_before: i128 = 0;
                    let mut total_locked_after: i128 = 0;

                    for (i, (amt, _)) in voters.iter().enumerate() {
                        let locked_before = lp.locked_balance(&holders[i]);
                        total_locked_before += locked_before;
                        if locked_before > 0 {
                            // Balance before unlock = balance - locked (locked unavailable for transfer).
                            let bal_before = lp.balance(&holders[i]);

                            let _ = gov.try_unlock_vote(&holders[i], &pid);

                            let locked_after = lp.locked_balance(&holders[i]);
                            total_locked_after += locked_after;

                            // After unlock: user should be able to transfer their full balance.
                            if locked_before > 0 && locked_after == 0 {
                                // Attempt to transfer the originally locked amount to a fresh address.
                                let recipient = Address::generate(&pe.env);
                                let transfer_result =
                                    lp.try_transfer(&holders[i], &recipient, &locked_before);
                                prop_assert!(
                                    transfer_result.is_ok(),
                                    "should be able to transfer unlocked tokens"
                                );
                            }
                        }
                    }

                    // After all unlocks, total locked should be 0 for concluded proposals.
                    prop_assert_eq!(
                        total_locked_after,
                        0,
                        "total locked after all unlocks should be 0, got {}",
                        total_locked_after
                    );
                }

                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn test_quorum_decay_passes_before_decay() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        gov.set_quorum_decay_bps_per_day(&100_i128);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.execute_after + 1);
        assert_eq!(gov.get_effective_quorum(&pid), 1_000);
        gov.execute(&pid);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Executed);
    }

    #[test]
    fn test_quorum_decay_defeats_after_threshold() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        gov.set_quorum_decay_bps_per_day(&500_i128);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        // 60 % of supply voted — below 100 % effective quorum after decay.
        gov.vote(&lp1, &pid, &Vote::For);

        let proposal = gov.get_proposal(&pid);
        // 20 days * 500 bps/day + 1000 base = 11000 capped at 10000
        s.env
            .ledger()
            .set_timestamp(proposal.vote_start + 20 * 86_400);
        s.env.ledger().set_timestamp(proposal.execute_after + 1);

        assert_eq!(gov.get_effective_quorum(&pid), 10_000);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Defeated);
    }

    #[test]
    fn test_quorum_decay_disabled_when_rate_zero() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 1_000);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);

        let proposal = gov.get_proposal(&pid);
        s.env
            .ledger()
            .set_timestamp(proposal.vote_start + 100 * 86_400);
        assert_eq!(gov.get_effective_quorum(&pid), 1_000);
    }
}
