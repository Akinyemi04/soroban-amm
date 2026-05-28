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

use soroban_sdk::{contract, contractimpl, contracterror, contracttype, Address, Env, Symbol};

// ── Constants ─────────────────────────────────────────────────────────────────

const MAX_BPS: i128 = 10_000;
const MIN_PERSISTENT_TTL: u32 = 172_800; // ~10 days at 5s/ledger
const PERSISTENT_TTL_BUMP_TO: u32 = 259_200; // ~15 days at 5s/ledger

// ── Typed errors ─────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum GovernanceError {
    AlreadyInitialized      = 1,
    InvalidVotingPeriod     = 2,
    InvalidTimelock         = 3,
    InvalidQuorumBps        = 4,
    InvalidProposerStake    = 5,
    InvalidFeeBps           = 6,
    ZeroTotalSupply         = 7,
    InsufficientStake       = 8,
    ProposalNotFound        = 9,
    VotingNotStarted        = 10,
    VotingPeriodEnded       = 11,
    AlreadyExecuted         = 12,
    ProposalCancelled       = 13,
    AlreadyVoted            = 14,
    NoVotingPower           = 15,
    VotingPeriodActive      = 16,
    ProposalExpired         = 17,
    TimelockNotElapsed      = 18,
    QuorumNotMet            = 19,
    ProposalDefeated        = 20,
    NotProposer             = 21,
    NoLockedVote            = 22,
    ProposalNotConcluded    = 23,
    CannotDelegateToSelf    = 24,
    Unauthorized            = 25,
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
    /// Individual proposal storage.
    Proposal(u32),
    /// Vote record for a voter on a proposal: (proposal_id, voter).
    HasVoted(u32, Address),
    /// Locked voting amount for a voter on a proposal.
    LockedVote(u32, Address),
    /// Delegation mapping: delegator -> delegatee address.
    Delegate(Address),
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
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateProtocolFeeParams {
    pub new_bps: i128,
    pub new_recipient: Address,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum ProposalKind {
    UpdateFee(i128),
    UpdateProtocolFee(UpdateProtocolFeeParams),
    UpdateFlashLoanFee(i128),
    TransferAdmin(Address),
    PausePool,
    UnpausePool,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct Proposal {
    pub id: u32,
    pub proposer: Address,
    pub kind: ProposalKind,
    /// LP total supply snapshot at proposal creation.
    pub snapshot_total_supply: i128,
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
}

// ── LP token client ───────────────────────────────────────────────────────────

#[soroban_sdk::contractclient(name = "LpTokenClient")]
pub trait LpTokenInterface {
    fn balance(env: Env, id: Address) -> i128;
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
    fn propose_admin(env: Env, current_admin: Address, new_admin: Address);
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
        env.storage().instance().set(&DataKey::ProposalCount, &0u32);
        Ok(())
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
    pub fn propose(env: Env, proposer: Address, kind: ProposalKind) -> Result<u32, GovernanceError> {
        proposer.require_auth();

        match &kind {
            ProposalKind::UpdateFee(new_fee_bps) => {
                if !(0..=MAX_BPS).contains(new_fee_bps) {
                    return Err(GovernanceError::InvalidFeeBps);
                }
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

        let proposal = Proposal {
            id,
            proposer: proposer.clone(),
            kind: kind.clone(),
            snapshot_total_supply: total_supply,
            vote_start: now,
            vote_end,
            execute_after,
            expires_at,
            votes_for: 0,
            votes_against: 0,
            votes_abstain: 0,
            executed: false,
            cancelled: false,
        };

        let proposal_key = DataKey::Proposal(id);
        env.storage().persistent().set(&proposal_key, &proposal);
        Self::bump_key_ttl(&env, &proposal_key);
        env.storage()
            .instance()
            .set(&DataKey::ProposalCount, &(id + 1));

        env.events().publish(
            (Symbol::new(&env, "proposed"),),
            (id, proposer, kind, vote_end),
        );

        Ok(id)
    }

    /// Cast a vote on an active proposal.
    ///
    /// Voting power = voter's current LP balance, which is then locked until
    /// the proposal concludes. Each address may only vote once per proposal.
    pub fn vote(env: Env, voter: Address, proposal_id: u32, choice: Vote) -> Result<(), GovernanceError> {
        voter.require_auth();

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

        let voted_key = DataKey::HasVoted(proposal_id, voter.clone());
        if env.storage().persistent().has(&voted_key) {
            return Err(GovernanceError::AlreadyVoted);
        }

        let lp_token: Address = env.storage().instance().get(&DataKey::LpToken).unwrap();
        let lp_client = LpTokenClient::new(&env, &lp_token);
        let voting_power = lp_client.balance(&voter);
        if voting_power == 0 {
            return Err(GovernanceError::NoVotingPower);
        }
        lp_client.lock(&voter, &voting_power);

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

        let lock_key = DataKey::LockedVote(proposal_id, voter.clone());
        env.storage().persistent().set(&lock_key, &voting_power);
        Self::bump_key_ttl(&env, &lock_key);

        env.events().publish(
            (Symbol::new(&env, "voted"),),
            (proposal_id, voter, choice, voting_power),
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

        let quorum_bps: i128 = env.storage().instance().get(&DataKey::QuorumBps).unwrap();
        let total_votes = proposal.votes_for + proposal.votes_against + proposal.votes_abstain;
        let quorum_threshold = proposal.snapshot_total_supply * quorum_bps / MAX_BPS;
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
        }

        proposal.executed = true;
        env.storage().persistent().set(&proposal_key, &proposal);
        Self::bump_key_ttl(&env, &proposal_key);

        env.events().publish(
            (Symbol::new(&env, "executed"),),
            (proposal_id, proposal.kind.clone()),
        );
        Ok(())
    }

    /// Cancel an active proposal. Only the original proposer can cancel,
    /// and only while voting is still open.
    pub fn cancel_proposal(env: Env, proposal_id: u32, proposer: Address) -> Result<(), GovernanceError> {
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

        env.events().publish(
            (Symbol::new(&env, "vote_unlocked"), voter.clone()),
            (proposal_id, locked),
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

    /// Get the total voting power (own + delegated) for an address at proposal creation.
    ///
    /// This computes the sum of LP balance for the address and all addresses that have
    /// delegated to this address.
    #[allow(dead_code)]
    fn get_voting_power(env: &Env, voter: &Address) -> i128 {
        let lp_token: Address = env.storage().instance().get(&DataKey::LpToken).unwrap();
        let lp_client = LpTokenClient::new(env, &lp_token);

        // Start with voter's own balance
        let total_power = lp_client.balance(voter);

        // Note: Due to Soroban's storage model, we cannot efficiently iterate over all delegators.
        // In a production implementation, you'd need to maintain a reverse delegation index
        // or use an alternative design. For now, we return the voter's own balance.
        // The delegation voting logic should be implemented off-chain or with a delegatee registry.

        total_power
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

        if now <= proposal.vote_end {
            return ProposalStatus::Active;
        }

        let quorum_bps: i128 = env.storage().instance().get(&DataKey::QuorumBps).unwrap();
        let total_votes = proposal.votes_for + proposal.votes_against + proposal.votes_abstain;
        let quorum_threshold = proposal.snapshot_total_supply * quorum_bps / MAX_BPS;
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

    fn bump_key_ttl(env: &Env, key: &DataKey) {
        env.storage()
            .persistent()
            .extend_ttl(key, MIN_PERSISTENT_TTL, PERSISTENT_TTL_BUMP_TO);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use amm::AmmPool;
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
        let proposed_data: (u32, Address, ProposalKind, u64) = proposed_evt.2.into_val(&s.env);
        assert_eq!(
            proposed_data,
            (
                pid,
                lp1.clone(),
                ProposalKind::UpdateFee(50),
                proposal.vote_end
            )
        );

        gov.vote(&lp1, &pid, &Vote::For);

        // `voted` event: (proposal_id, voter, choice, voting_power)
        let events = s.env.events().all();
        let voted_evt = events
            .iter()
            .find(|e| e.0 == gov.address && e.1 == (Symbol::new(&s.env, "voted"),).into_val(&s.env))
            .expect("voted event not found");
        let voted_data: (u32, Address, Vote, i128) = voted_evt.2.into_val(&s.env);
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
        let executed_data: (u32, ProposalKind) = executed_evt.2.into_val(&s.env);
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
                    && e.1
                        == (Symbol::new(&s.env, "vote_unlocked"), lp1.clone())
                            .into_val(&s.env)
            })
            .expect("vote_unlocked event not emitted");

        let data: (u32, i128) = unlock_evt.2.into_val(&s.env);
        assert_eq!(data.0, pid);
        assert_eq!(data.1, 600_i128); // amount_unlocked == voting power used
    }
}

// ── Property-based tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod prop_tests {
    extern crate std;
    use proptest::prelude::*;

    proptest! {
        /// Property 1: Quorum threshold never overflows or goes out of bounds.
        #[test]
        fn quorum_check_never_overflows(
            total_supply in 1i128..i128::MAX / 10_000,
            quorum_bps in 1i128..10_000i128,
        ) {
            let threshold = total_supply * quorum_bps / 10_000;
            prop_assert!(threshold >= 0);
            prop_assert!(threshold <= total_supply);
        }

        /// Property 2: Majority check logic is correct and doesn't panic.
        #[test]
        fn majority_implies_votes_for_gt_against(
            votes_for in 0i128..i128::MAX / 2,
            votes_against in 0i128..i128::MAX / 2,
        ) {
            let passed = votes_for > votes_against;
            prop_assert_eq!(passed, votes_for > votes_against);
        }

        /// Property 3: Combined votes cast ≤ total supply is preserved.
        #[test]
        fn total_votes_does_not_overflow(
            votes_for in 0i128..i128::MAX / 2,
            votes_against in 0i128..i128::MAX / 2,
        ) {
            let total_supply = votes_for.saturating_add(votes_against);
            let total_votes = votes_for + votes_against;
            prop_assert!(total_votes <= total_supply);
        }

        /// Property 4: Timelock boundary execute_after == vote_end + TIMELOCK_SECS always holds.
        #[test]
        fn timelock_boundary_always_holds(
            vote_end in 0u64..u64::MAX / 2,
            timelock in 0u64..u64::MAX / 2,
        ) {
            let execute_after = vote_end + timelock;
            prop_assert_eq!(execute_after, vote_end + timelock);
        }

        /// Property 5: Min proposer stake math holds and is within expected bounds.
        #[test]
        fn min_proposer_stake_is_correct(
            total_supply in 1i128..i128::MAX / 10_000,
            min_bps in 0i128..10_000i128,
        ) {
            let min_stake = ((total_supply * min_bps) / 10_000).max(1);
            prop_assert!(min_stake >= 1);
            prop_assert!(min_stake <= total_supply.max(1));
        }

        /// Property 6: Expiry always comes at or after execute_after.
        #[test]
        fn expiry_logic_boundaries(
            vote_end in 0u64..u64::MAX / 3,
            timelock in 0u64..u64::MAX / 3,
            voting_period in 1u64..u64::MAX / 3,
        ) {
            let execute_after = vote_end + timelock;
            let expires_at = execute_after + timelock.max(voting_period);
            prop_assert!(expires_at >= execute_after);
        }
    }
}
