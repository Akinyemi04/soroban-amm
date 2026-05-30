//! Liquidity reserve management contract.
//!
//! Tracks protocol-wide minimum liquidity requirements for pool pairs and
//! exposes a `check_reserves` guard that other contracts can call before
//! processing withdrawals or rebalancing operations.
//!
//! Governance is a single address that may update requirements. The address
//! can be a multisig or DAO contract for on-chain governance.
//!
//! Flow:
//!   1. Deploy this contract.
//!   2. Call `initialize` with the governance address and the factory address.
//!   3. Governance calls `set_min_reserve` to configure per-pair requirements.
//!   4. Any caller uses `check_reserves` to verify a pool is compliant.
//!   5. Governance may call `transfer_governance` to hand off control.

#![no_std]

use soroban_sdk::{contract, contractclient, contractimpl, contracttype, Address, Env};

// ── External contract interfaces ─────────────────────────────────────────────

/// Subset of the AMM pool interface needed to read current reserves.
#[contractclient(name = "AmmPoolClient")]
pub trait AmmPoolInterface {
    fn get_info(env: Env) -> PoolInfo;
}

/// Mirror of the PoolInfo struct exported by the AMM pool contract.
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
}

/// Minimum reserve requirement for a token pair.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct ReserveRequirement {
    pub min_reserve_a: i128,
    pub min_reserve_b: i128,
}

// ── Storage keys ─────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    Governance,
    Factory,
    /// Normalized (smaller_addr, larger_addr) → ReserveRequirement.
    MinReserve(Address, Address),
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct ReserveManager;

#[contractimpl]
impl ReserveManager {
    // ── Setup ─────────────────────────────────────────────────────────────────

    /// One-time setup. `governance` is the only address permitted to call
    /// `set_min_reserve` and `transfer_governance`.
    pub fn initialize(env: Env, governance: Address, factory: Address) {
        assert!(
            !env.storage().instance().has(&DataKey::Governance),
            "already initialized"
        );
        env.storage().instance().set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Factory, &factory);
    }

    // ── Governance ────────────────────────────────────────────────────────────

    /// Transfer governance to a new address. Requires current governance auth.
    pub fn transfer_governance(env: Env, new_governance: Address) {
        let gov: Address = env.storage().instance().get(&DataKey::Governance).unwrap();
        gov.require_auth();
        env.storage().instance().set(&DataKey::Governance, &new_governance);
    }

    // ── Reserve requirements ──────────────────────────────────────────────────

    /// Set the minimum reserve amounts for a token pair.
    ///
    /// Requires governance auth. Token order is normalised: the pair is stored
    /// with the lexicographically smaller address first so that lookups are
    /// order-independent.
    ///
    /// Set both values to 0 to remove a requirement.
    pub fn set_min_reserve(
        env: Env,
        token_a: Address,
        token_b: Address,
        min_reserve_a: i128,
        min_reserve_b: i128,
    ) {
        let gov: Address = env.storage().instance().get(&DataKey::Governance).unwrap();
        gov.require_auth();
        assert!(min_reserve_a >= 0, "min_reserve_a must be non-negative");
        assert!(min_reserve_b >= 0, "min_reserve_b must be non-negative");

        let (ta, tb) = Self::normalize(token_a, token_b);
        let req = ReserveRequirement { min_reserve_a, min_reserve_b };
        env.storage()
            .instance()
            .set(&DataKey::MinReserve(ta, tb), &req);
    }

    /// Return the minimum reserve requirement for a pair, or (0, 0) if none.
    pub fn get_min_reserve(
        env: Env,
        token_a: Address,
        token_b: Address,
    ) -> ReserveRequirement {
        let (ta, tb) = Self::normalize(token_a, token_b);
        env.storage()
            .instance()
            .get(&DataKey::MinReserve(ta, tb))
            .unwrap_or(ReserveRequirement {
                min_reserve_a: 0,
                min_reserve_b: 0,
            })
    }

    // ── Compliance checks ─────────────────────────────────────────────────────

    /// Check whether a pool's current reserves satisfy the registered minimums.
    ///
    /// Returns `true` if the pool meets or exceeds its requirements, or if no
    /// requirement has been set for that pair. Returns `false` otherwise.
    ///
    /// Does not modify any state.
    pub fn check_reserves(env: Env, pool: Address) -> bool {
        let info = AmmPoolClient::new(&env, &pool).get_info();
        let (ta, tb) = Self::normalize(info.token_a, info.token_b);

        let req: ReserveRequirement = env
            .storage()
            .instance()
            .get(&DataKey::MinReserve(ta, tb))
            .unwrap_or(ReserveRequirement {
                min_reserve_a: 0,
                min_reserve_b: 0,
            });

        info.reserve_a >= req.min_reserve_a && info.reserve_b >= req.min_reserve_b
    }

    /// Return the governance address.
    pub fn get_governance(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Governance).unwrap()
    }

    /// Return the factory address.
    pub fn get_factory(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Factory).unwrap()
    }

    // ── Internals ─────────────────────────────────────────────────────────────

    fn normalize(a: Address, b: Address) -> (Address, Address) {
        if a < b { (a, b) } else { (b, a) }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::Address as _,
        token::{StellarAssetClient, TokenClient as StellarTokenClient},
        Env,
    };

    mod amm_wasm {
        soroban_sdk::contractimport!(
            file = "../../target/wasm32-unknown-unknown/release/amm.wasm"
        );
    }

    mod token_wasm {
        soroban_sdk::contractimport!(
            file = "../../target/wasm32-unknown-unknown/release/token.wasm"
        );
    }

    mod factory_wasm {
        soroban_sdk::contractimport!(
            file = "../../target/wasm32-unknown-unknown/release/factory.wasm"
        );
    }

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

    struct Setup {
        env: Env,
        rm_addr: Address,
        pool: Address,
        ta: Address,
        tb: Address,
        governance: Address,
    }

    fn setup() -> Setup {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1000);

        let admin = Address::generate(&env);
        let governance = Address::generate(&env);

        let amm_hash = env.deployer().upload_contract_wasm(amm_wasm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token_wasm::WASM);

        let factory_addr = env.register_contract(None, factory_wasm::Factory);
        let factory = factory_wasm::FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let (ta_client, ta_sac) = create_sac(&env, &admin);
        let (tb_client, tb_sac) = create_sac(&env, &admin);

        let pool = factory.create_pool(&ta_client.address, &tb_client.address, &30_i128);

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);

        let amm = amm_wasm::Client::new(&env, &pool);
        amm.add_liquidity(&provider, &1_000_000_i128, &1_000_000_i128, &0_i128);

        let rm_addr = env.register_contract(None, ReserveManager);
        ReserveManagerClient::new(&env, &rm_addr)
            .initialize(&governance, &factory_addr);

        Setup {
            env,
            rm_addr,
            pool,
            ta: ta_client.address,
            tb: tb_client.address,
            governance,
        }
    }

    #[test]
    fn test_initialize_stores_governance_and_factory() {
        let env = Env::default();
        env.mock_all_auths();
        let gov = Address::generate(&env);
        let factory = Address::generate(&env);
        let rm_addr = env.register_contract(None, ReserveManager);
        let rm = ReserveManagerClient::new(&env, &rm_addr);
        rm.initialize(&gov, &factory);
        assert_eq!(rm.get_governance(), gov);
        assert_eq!(rm.get_factory(), factory);
    }

    #[test]
    fn test_initialize_twice_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let gov = Address::generate(&env);
        let factory = Address::generate(&env);
        let rm_addr = env.register_contract(None, ReserveManager);
        let rm = ReserveManagerClient::new(&env, &rm_addr);
        rm.initialize(&gov, &factory);
        assert!(rm.try_initialize(&gov, &factory).is_err());
    }

    #[test]
    fn test_set_and_get_min_reserve() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);

        rm.set_min_reserve(&s.ta, &s.tb, &500_000_i128, &300_000_i128);

        // Order-independent lookup
        let req_ab = rm.get_min_reserve(&s.ta, &s.tb);
        let req_ba = rm.get_min_reserve(&s.tb, &s.ta);

        // Reserves are stored normalised; values correspond to the normalised order
        assert_eq!(req_ab.min_reserve_a, req_ba.min_reserve_a);
        assert_eq!(req_ab.min_reserve_b, req_ba.min_reserve_b);
    }

    #[test]
    fn test_check_reserves_passes_when_above_minimum() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);

        // Pool has 1_000_000 of each; set minimum below that
        rm.set_min_reserve(&s.ta, &s.tb, &500_000_i128, &500_000_i128);
        assert!(rm.check_reserves(&s.pool));
    }

    #[test]
    fn test_check_reserves_fails_when_below_minimum() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);

        // Set minimum above current reserves (1_000_000)
        rm.set_min_reserve(&s.ta, &s.tb, &2_000_000_i128, &2_000_000_i128);
        assert!(!rm.check_reserves(&s.pool));
    }

    #[test]
    fn test_check_reserves_passes_with_no_requirement() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);

        // No requirement set — should pass by default
        assert!(rm.check_reserves(&s.pool));
    }

    #[test]
    fn test_transfer_governance() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        let new_gov = Address::generate(&s.env);

        rm.transfer_governance(&new_gov);
        assert_eq!(rm.get_governance(), new_gov);
    }

    #[test]
    fn test_set_min_reserve_to_zero_removes_constraint() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);

        rm.set_min_reserve(&s.ta, &s.tb, &2_000_000_i128, &2_000_000_i128);
        assert!(!rm.check_reserves(&s.pool));

        rm.set_min_reserve(&s.ta, &s.tb, &0_i128, &0_i128);
        assert!(rm.check_reserves(&s.pool));
    }

    #[test]
    fn test_negative_min_reserve_panics() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        assert!(rm.try_set_min_reserve(&s.ta, &s.tb, &-1_i128, &0_i128).is_err());
    }
}
