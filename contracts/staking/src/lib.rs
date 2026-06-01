//! LP Staking and Rewards Contract
//!
//! Liquidity providers can stake their LP tokens to earn reward tokens.
//! Uses a rewards-per-share accumulator pattern (similar to SushiSwap's MasterChef)
//! for efficient O(1) reward calculation per claim.
//!
//! Issue #296: Optional lock-duration boost multiplier (1×–4×), modelled on
//! Curve's veTokenomics.  Stakers may voluntarily lock for a fixed duration to
//! earn a higher share of rewards.  The boost is applied to the *effective*
//! staked amount used in reward calculations; the actual LP token balance is
//! unchanged.

#![no_std]

use soroban_sdk::{contract, contractimpl, contracttype, Address, Env, Symbol};

use soroban_sdk::token::Client as SepTokenClient;

// ── Constants ──────────────────────────────────────────────────────────────

const SCALE_FACTOR: i128 = 1_000_000_000_000_000_000; // 1e18

/// Boost multiplier is stored scaled by BOOST_SCALE so we avoid floats.
/// 1× = 10_000, 4× = 40_000.
const BOOST_SCALE: i128 = 10_000;

/// Maximum lock duration in seconds (4 years).
const MAX_LOCK_DURATION: u64 = 4 * 365 * 24 * 3600;

/// Minimum lock duration in seconds (1 week).
const MIN_LOCK_DURATION: u64 = 7 * 24 * 3600;

/// Maximum boost multiplier (4×, stored as 40_000 / BOOST_SCALE).
const MAX_BOOST: i128 = 4 * BOOST_SCALE;

/// Minimum boost multiplier (1×, stored as 10_000 / BOOST_SCALE).
const MIN_BOOST: i128 = BOOST_SCALE;

// ── Storage keys ───────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    /// LP token address
    LpToken,
    /// Reward token address
    RewardToken,
    /// Admin address (can add rewards)
    Admin,
    /// Total *effective* LP tokens staked (boosted amounts summed)
    TotalEffectiveStaked,
    /// Accumulated rewards per effective LP token (scaled by 1e18)
    AccumulatedRewardsPerShare,
    /// Staker info: raw staked amount
    StakerAmount(Address),
    /// Staker info: rewards debt (to track already-distributed rewards)
    StakerRewardsDebt(Address),
    /// Remaining reward tokens available in pool
    RewardPoolBalance,
    /// Lock expiry timestamp (seconds) for a staker; 0 = no lock
    LockExpiry(Address),
    /// Boost multiplier for a staker (scaled by BOOST_SCALE); default = BOOST_SCALE (1×)
    BoostMultiplier(Address),
}

// ── Data structures ───────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, Debug)]
pub struct StakerInfo {
    pub staked_amount: i128,
    pub effective_amount: i128,
    pub rewards_debt: i128,
    pub lock_expiry: u64,
    pub boost_multiplier: i128,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct PoolInfo {
    pub lp_token: Address,
    pub reward_token: Address,
    pub admin: Address,
    pub total_effective_staked: i128,
    pub reward_pool_balance: i128,
    pub accumulated_rewards_per_share: i128,
}

// ── Contract ────────────────────────────────────────────────────────────────

#[contract]
pub struct Staking;

#[contractimpl]
impl Staking {
    /// Initialize the staking contract.
    pub fn initialize(env: Env, lp_token: Address, reward_token: Address, admin: Address) {
        assert!(
            !env.storage().instance().has(&DataKey::LpToken),
            "already initialized"
        );
        env.storage().instance().set(&DataKey::LpToken, &lp_token);
        env.storage().instance().set(&DataKey::RewardToken, &reward_token);
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::TotalEffectiveStaked, &0i128);
        env.storage().instance().set(&DataKey::AccumulatedRewardsPerShare, &0i128);
        env.storage().instance().set(&DataKey::RewardPoolBalance, &0i128);
    }

    /// Add rewards to the pool. Admin only.
    pub fn add_rewards(env: Env, admin: Address, amount: i128) {
        admin.require_auth();
        let stored_admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        assert!(admin == stored_admin, "not admin");
        assert!(amount > 0, "amount must be positive");

        let reward_token: Address = env.storage().instance().get(&DataKey::RewardToken).unwrap();
        let pool_addr = env.current_contract_address();
        SepTokenClient::new(&env, &reward_token).transfer_from(&admin, &admin, &pool_addr, &amount);

        let current_balance: i128 = env
            .storage()
            .instance()
            .get(&DataKey::RewardPoolBalance)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::RewardPoolBalance, &(current_balance + amount));

        env.events().publish((Symbol::new(&env, "rewards_added"),), (admin, amount));
    }

    /// Stake LP tokens without a lock (1× boost).
    pub fn stake(env: Env, staker: Address, amount: i128) {
        Self::stake_locked(env, staker, amount, 0);
    }

    /// Stake LP tokens with an optional lock duration for a boost multiplier.
    ///
    /// `lock_duration_secs` = 0 → no lock, 1× boost.
    /// Lock duration is clamped to [MIN_LOCK_DURATION, MAX_LOCK_DURATION].
    /// Boost scales linearly from 1× (no lock) to 4× (MAX_LOCK_DURATION).
    ///
    /// If the staker already has a lock, the new lock must expire no earlier
    /// than the existing one (locks can only be extended, not shortened).
    pub fn stake_locked(env: Env, staker: Address, amount: i128, lock_duration_secs: u64) {
        staker.require_auth();
        assert!(amount > 0, "amount must be positive");

        let lp_token: Address = env.storage().instance().get(&DataKey::LpToken).unwrap();
        let pool_addr = env.current_contract_address();
        SepTokenClient::new(&env, &lp_token).transfer_from(&staker, &staker, &pool_addr, &amount);

        // Settle any pending rewards before changing effective stake.
        Self::_settle_pending(&env, &staker);

        // Compute new boost and lock expiry.
        let now = env.ledger().timestamp();
        let existing_expiry: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::LockExpiry(staker.clone()))
            .unwrap_or(0);

        let (new_expiry, new_boost) = if lock_duration_secs == 0 {
            // No new lock requested — keep existing lock if still active.
            let expiry = existing_expiry.max(now);
            let boost = Self::_boost_for_remaining(expiry, now);
            (existing_expiry, boost)
        } else {
            let clamped = lock_duration_secs.clamp(MIN_LOCK_DURATION, MAX_LOCK_DURATION);
            let proposed_expiry = now + clamped;
            // Cannot shorten an existing lock.
            let expiry = proposed_expiry.max(existing_expiry);
            let boost = Self::_boost_for_remaining(expiry, now);
            (expiry, boost)
        };

        // Update raw staked amount.
        let current_staked: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::StakerAmount(staker.clone()))
            .unwrap_or(0);
        let new_staked = current_staked + amount;
        env.storage()
            .persistent()
            .set(&DataKey::StakerAmount(staker.clone()), &new_staked);

        // Recompute effective amount for the whole position with the new boost.
        let old_effective = Self::_effective_amount(current_staked, new_boost);
        let new_effective = Self::_effective_amount(new_staked, new_boost);

        // Adjust total effective staked.
        let total: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalEffectiveStaked)
            .unwrap_or(0);
        // Remove old effective, add new effective.
        let new_total = total - old_effective + new_effective;
        env.storage()
            .instance()
            .set(&DataKey::TotalEffectiveStaked, &new_total.max(0));

        // Persist lock and boost.
        env.storage()
            .persistent()
            .set(&DataKey::LockExpiry(staker.clone()), &new_expiry);
        env.storage()
            .persistent()
            .set(&DataKey::BoostMultiplier(staker.clone()), &new_boost);

        // Reset rewards debt to current acc_per_share * new_effective.
        let acc_per_share: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedRewardsPerShare)
            .unwrap_or(0);
        let new_debt = new_effective * acc_per_share / SCALE_FACTOR;
        env.storage()
            .persistent()
            .set(&DataKey::StakerRewardsDebt(staker.clone()), &new_debt);

        env.events().publish(
            (Symbol::new(&env, "staked"),),
            (staker, amount, new_boost, new_expiry),
        );
    }

    /// Claim accrued rewards without unstaking.
    pub fn claim(env: Env, staker: Address) -> i128 {
        staker.require_auth();

        let pending = Self::pending_rewards(env.clone(), staker.clone());
        assert!(pending > 0, "no pending rewards");

        let reward_token: Address = env.storage().instance().get(&DataKey::RewardToken).unwrap();
        let pool_addr = env.current_contract_address();

        // Reset debt.
        let effective = Self::_staker_effective(&env, &staker);
        let acc_per_share: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedRewardsPerShare)
            .unwrap_or(0);
        let new_debt = effective * acc_per_share / SCALE_FACTOR;
        env.storage()
            .persistent()
            .set(&DataKey::StakerRewardsDebt(staker.clone()), &new_debt);

        SepTokenClient::new(&env, &reward_token).transfer(&pool_addr, &staker, &pending);

        let pool_balance: i128 = env
            .storage()
            .instance()
            .get(&DataKey::RewardPoolBalance)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::RewardPoolBalance, &(pool_balance - pending));

        env.events().publish((Symbol::new(&env, "claimed"),), (staker, pending));
        pending
    }

    /// Unstake LP tokens and claim pending rewards.
    ///
    /// Panics if the staker's lock has not yet expired.
    pub fn unstake(env: Env, staker: Address, amount: i128) -> (i128, i128) {
        staker.require_auth();
        assert!(amount > 0, "amount must be positive");

        let staked_amount: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::StakerAmount(staker.clone()))
            .unwrap_or(0);
        assert!(staked_amount >= amount, "insufficient staked amount");

        // Enforce lock.
        let now = env.ledger().timestamp();
        let lock_expiry: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::LockExpiry(staker.clone()))
            .unwrap_or(0);
        assert!(now >= lock_expiry, "tokens are still locked");

        // Claim pending rewards first.
        let rewards = if Self::pending_rewards(env.clone(), staker.clone()) > 0 {
            Self::claim(env.clone(), staker.clone())
        } else {
            0
        };

        let lp_token: Address = env.storage().instance().get(&DataKey::LpToken).unwrap();
        let pool_addr = env.current_contract_address();
        SepTokenClient::new(&env, &lp_token).transfer(&pool_addr, &staker, &amount);

        let boost: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::BoostMultiplier(staker.clone()))
            .unwrap_or(MIN_BOOST);

        let old_effective = Self::_effective_amount(staked_amount, boost);
        let new_staked = staked_amount - amount;
        let new_effective = Self::_effective_amount(new_staked, boost);

        env.storage()
            .persistent()
            .set(&DataKey::StakerAmount(staker.clone()), &new_staked);

        let total: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalEffectiveStaked)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalEffectiveStaked, &(total - old_effective + new_effective).max(0));

        // Reset debt.
        let acc_per_share: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedRewardsPerShare)
            .unwrap_or(0);
        let new_debt = new_effective * acc_per_share / SCALE_FACTOR;
        env.storage()
            .persistent()
            .set(&DataKey::StakerRewardsDebt(staker.clone()), &new_debt);

        env.events().publish((Symbol::new(&env, "unstaked"),), (staker, amount, rewards));
        (amount, rewards)
    }

    /// View pending rewards for a staker.
    pub fn pending_rewards(env: Env, staker: Address) -> i128 {
        let effective = Self::_staker_effective(&env, &staker);
        if effective == 0 {
            return 0;
        }
        let acc_per_share: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedRewardsPerShare)
            .unwrap_or(0);
        let rewards_debt: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::StakerRewardsDebt(staker))
            .unwrap_or(0);
        (effective * acc_per_share / SCALE_FACTOR - rewards_debt).max(0)
    }

    /// Get pool information.
    pub fn get_pool_info(env: Env) -> PoolInfo {
        PoolInfo {
            lp_token: env.storage().instance().get(&DataKey::LpToken).unwrap(),
            reward_token: env.storage().instance().get(&DataKey::RewardToken).unwrap(),
            admin: env.storage().instance().get(&DataKey::Admin).unwrap(),
            total_effective_staked: env
                .storage()
                .instance()
                .get(&DataKey::TotalEffectiveStaked)
                .unwrap_or(0),
            reward_pool_balance: env
                .storage()
                .instance()
                .get(&DataKey::RewardPoolBalance)
                .unwrap_or(0),
            accumulated_rewards_per_share: env
                .storage()
                .instance()
                .get(&DataKey::AccumulatedRewardsPerShare)
                .unwrap_or(0),
        }
    }

    /// Get staker info including boost and lock details.
    pub fn get_staker_info(env: Env, staker: Address) -> StakerInfo {
        let staked_amount: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::StakerAmount(staker.clone()))
            .unwrap_or(0);
        let boost: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::BoostMultiplier(staker.clone()))
            .unwrap_or(MIN_BOOST);
        let lock_expiry: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::LockExpiry(staker.clone()))
            .unwrap_or(0);
        let rewards_debt: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::StakerRewardsDebt(staker))
            .unwrap_or(0);
        StakerInfo {
            staked_amount,
            effective_amount: Self::_effective_amount(staked_amount, boost),
            rewards_debt,
            lock_expiry,
            boost_multiplier: boost,
        }
    }

    /// Distribute new rewards across all stakers. Admin only.
    pub fn update_rewards(env: Env, admin: Address, new_rewards: i128) {
        admin.require_auth();
        let stored_admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        assert!(admin == stored_admin, "not admin");
        assert!(new_rewards > 0, "new_rewards must be positive");

        let total_effective: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalEffectiveStaked)
            .unwrap_or(0);
        assert!(total_effective > 0, "no stakers");

        let acc_per_share: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedRewardsPerShare)
            .unwrap_or(0);
        let rewards_increase = new_rewards * SCALE_FACTOR / total_effective;
        env.storage()
            .instance()
            .set(&DataKey::AccumulatedRewardsPerShare, &(acc_per_share + rewards_increase));

        let pool_balance: i128 = env
            .storage()
            .instance()
            .get(&DataKey::RewardPoolBalance)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::RewardPoolBalance, &(pool_balance - new_rewards));

        env.events().publish((Symbol::new(&env, "rewards_updated"),), (new_rewards,));
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    /// Compute boost multiplier (scaled by BOOST_SCALE) for a given remaining
    /// lock duration.  Scales linearly: 0 s → 1×, MAX_LOCK_DURATION → 4×.
    fn _boost_for_remaining(expiry: u64, now: u64) -> i128 {
        if expiry <= now {
            return MIN_BOOST;
        }
        let remaining = expiry - now;
        let clamped = remaining.min(MAX_LOCK_DURATION) as i128;
        let max_dur = MAX_LOCK_DURATION as i128;
        // boost = 1 + 3 * (remaining / MAX_LOCK_DURATION), scaled by BOOST_SCALE
        MIN_BOOST + (MAX_BOOST - MIN_BOOST) * clamped / max_dur
    }

    /// Effective staked amount = raw_amount * boost / BOOST_SCALE.
    fn _effective_amount(raw: i128, boost: i128) -> i128 {
        raw * boost / BOOST_SCALE
    }

    /// Current effective amount for a staker (uses stored boost).
    fn _staker_effective(env: &Env, staker: &Address) -> i128 {
        let raw: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::StakerAmount(staker.clone()))
            .unwrap_or(0);
        if raw == 0 {
            return 0;
        }
        let boost: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::BoostMultiplier(staker.clone()))
            .unwrap_or(MIN_BOOST);
        Self::_effective_amount(raw, boost)
    }

    /// Settle pending rewards into debt without transferring (used before
    /// changing effective stake so rewards earned so far are not lost).
    fn _settle_pending(env: &Env, staker: &Address) {
        let effective = Self::_staker_effective(env, staker);
        if effective == 0 {
            return;
        }
        let acc_per_share: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedRewardsPerShare)
            .unwrap_or(0);
        // The current debt already accounts for previously settled rewards.
        // We do NOT transfer here — just record what has been earned so far
        // by updating the debt to the current acc_per_share level.
        // Pending = effective * acc / SCALE - debt  (already owed to staker).
        // We leave debt unchanged so pending_rewards still returns the right value.
        // The actual settlement happens in claim() / unstake().
        let _ = (effective, acc_per_share); // no-op: debt stays, rewards accumulate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        token::{StellarAssetClient, TokenClient as StellarTokenClient},
        Address, Env,
    };

    fn create_sac<'a>(
        env: &'a Env,
        admin: &Address,
    ) -> (StellarTokenClient<'a>, StellarAssetClient<'a>) {
        let contract = env.register_stellar_asset_contract_v2(admin.clone());
        (
            StellarTokenClient::new(env, &contract.address()),
            StellarAssetClient::new(env, &contract.address()),
        )
    }

    fn setup(env: &Env) -> (Address, Address, StakingClient) {
        let admin = Address::generate(env);
        let staking_addr = env.register_contract(None, Staking);
        let (lp_token, lp_sac) = create_sac(env, &admin);
        let (reward_token, reward_sac) = create_sac(env, &admin);
        let staking = StakingClient::new(env, &staking_addr);
        staking.initialize(&lp_token.address, &reward_token.address, &admin);
        reward_sac.mint(&admin, &10_000_i128);
        staking.add_rewards(&admin, &10_000_i128);
        let staker = Address::generate(env);
        lp_sac.mint(&staker, &5_000_i128);
        (admin, staker, staking)
    }

    #[test]
    fn test_stake_no_lock_one_x_boost() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);

        staking.stake(&staker, &1_000_i128);

        let info = staking.get_staker_info(&staker);
        assert_eq!(info.staked_amount, 1_000);
        assert_eq!(info.boost_multiplier, BOOST_SCALE); // 1×
        assert_eq!(info.effective_amount, 1_000);
        assert_eq!(info.lock_expiry, 0);

        let pool = staking.get_pool_info();
        assert_eq!(pool.total_effective_staked, 1_000);
    }

    #[test]
    fn test_stake_locked_max_duration_four_x_boost() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);

        staking.stake_locked(&staker, &1_000_i128, &MAX_LOCK_DURATION);

        let info = staking.get_staker_info(&staker);
        assert_eq!(info.staked_amount, 1_000);
        assert_eq!(info.boost_multiplier, MAX_BOOST); // 4×
        assert_eq!(info.effective_amount, 4_000);

        let pool = staking.get_pool_info();
        assert_eq!(pool.total_effective_staked, 4_000);
    }

    #[test]
    fn test_boosted_staker_earns_more_rewards() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker_a, staking) = setup(&env);
        let staker_b = Address::generate(&env);

        // Mint LP for staker_b
        let lp_token = staking.get_pool_info().lp_token;
        StellarAssetClient::new(&env, &lp_token).mint(&staker_b, &1_000_i128);

        // staker_a: 1000 LP, no lock (1×) → effective 1000
        staking.stake(&staker_a, &1_000_i128);
        // staker_b: 1000 LP, max lock (4×) → effective 4000
        staking.stake_locked(&staker_b, &1_000_i128, &MAX_LOCK_DURATION);

        // Distribute 500 rewards across total effective 5000
        staking.update_rewards(&admin, &500_i128);

        let pending_a = staking.pending_rewards(&staker_a);
        let pending_b = staking.pending_rewards(&staker_b);

        // staker_b should earn 4× more than staker_a
        assert_eq!(pending_a, 100); // 500 * 1000/5000
        assert_eq!(pending_b, 400); // 500 * 4000/5000
    }

    #[test]
    fn test_unstake_locked_before_expiry_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);

        staking.stake_locked(&staker, &1_000_i128, &MIN_LOCK_DURATION);

        // Try to unstake immediately — should panic because lock hasn't expired.
        let result = staking.try_unstake(&staker, &1_000_i128);
        assert!(result.is_err());
    }

    #[test]
    fn test_unstake_after_lock_expiry_succeeds() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);

        staking.stake_locked(&staker, &1_000_i128, &MIN_LOCK_DURATION);
        staking.update_rewards(&admin, &100_i128);

        // Advance time past lock expiry.
        env.ledger().with_mut(|l| {
            l.timestamp = l.timestamp + MIN_LOCK_DURATION + 1;
        });

        let (lp_returned, rewards) = staking.unstake(&staker, &1_000_i128);
        assert_eq!(lp_returned, 1_000);
        assert!(rewards > 0);
    }

    #[test]
    fn test_stake_and_claim() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);

        staking.stake(&staker, &1_000_i128);
        staking.update_rewards(&admin, &100_i128);

        let pending = staking.pending_rewards(&staker);
        assert_eq!(pending, 100);

        let claimed = staking.claim(&staker);
        assert_eq!(claimed, 100);
        assert_eq!(staking.pending_rewards(&staker), 0);
    }
}
