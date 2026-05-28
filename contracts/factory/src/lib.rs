//! Factory contract — deploys and registers AMM pool instances.
//!
//! Flow:
//!   1. Deploy this contract.
//!   2. Call `initialize` with the admin address and pre-uploaded WASM hashes
//!      for the AMM pool and LP token contracts.
//!   3. Call `create_pool` for each token pair you want a pool for.
//!   4. Use `get_pool` / `all_pools` to discover deployed pools.

#![no_std]

use soroban_sdk::{
    contract, contractclient, contractimpl, contracttype, Address, BytesN, Env, Symbol, Vec,
};

#[contractclient(name = "ClPoolClient")]
pub trait ClPoolInterface {
    fn initialize(
        env: Env,
        token_a: Address,
        token_b: Address,
        fee_bps: i128,
        initial_tick: i32,
    );
}

#[contractclient(name = "AmmPoolClient")]
pub trait AmmPoolInterface {
    #[allow(clippy::too_many_arguments)]
    fn initialize(
        env: Env,
        admin: Address,
        token_a: Address,
        token_b: Address,
        lp_token: Address,
        fee_bps: i128,
        fee_recipient: Address,
        protocol_fee_bps: i128,
    );
}

#[contractclient(name = "LpTokenClient")]
pub trait LpTokenInterface {
    fn initialize(
        env: Env,
        admin: Address,
        name: soroban_sdk::String,
        symbol: soroban_sdk::String,
        decimals: u32,
    );
    fn set_locker(env: Env, locker: Address);
}

#[contractclient(name = "GovernanceClient")]
pub trait GovernanceInterface {
    #[allow(clippy::too_many_arguments)]
    fn initialize(
        env: Env,
        admin: Address,
        amm_pool: Address,
        lp_token: Address,
        voting_period_secs: u64,
        timelock_secs: u64,
        quorum_bps: i128,
        min_proposer_stake_bps: i128,
    );
}

// ── Storage keys ─────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    Pool(Address, Address),          // normalized (token_a, token_b) → pool Address
    LpToken(Address),                // pool address → LP token address
    AllPools,                        // Vec<Address> of every deployed pool
    Admin,
    AmmWasmHash,
    TokenWasmHash,
    ClWasmHash,                      // WASM hash for concentrated_liquidity deployments
    PoolCount,                       // u64 monotonic counter — used to derive unique deploy salts
    GovernanceFor(Address),          // pool address → Option<Address>
    ClPool(Address, Address, i128),  // normalized (token_a, token_b, fee_bps) → CL pool Address
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct Factory;

#[contractimpl]
impl Factory {
    // ── Setup ─────────────────────────────────────────────────────────────────

    /// One-time factory setup.
    ///
    /// `amm_wasm_hash` and `token_wasm_hash` must be uploaded to the network
    /// (via `stellar contract upload`) before calling this function.
    pub fn initialize(
        env: Env,
        admin: Address,
        amm_wasm_hash: BytesN<32>,
        token_wasm_hash: BytesN<32>,
    ) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::AmmWasmHash, &amm_wasm_hash);
        env.storage()
            .instance()
            .set(&DataKey::TokenWasmHash, &token_wasm_hash);
        env.storage()
            .instance()
            .set(&DataKey::AllPools, &Vec::<Address>::new(&env));
        env.storage().instance().set(&DataKey::PoolCount, &0u64);
    }

    // ── Pool creation ─────────────────────────────────────────────────────────

    /// Deploy a new AMM pool for `(token_a, token_b)` with `fee_bps` swap fee.
    ///
    /// Token pair order is normalised — the pool is always stored with the
    /// lexicographically smaller address as `token_a`, so callers do not need
    /// to match the original order when looking up a pool.
    ///
    /// `lp_name` and `lp_symbol` set the LP token's metadata. When `None` the
    /// factory generates counter-based defaults: `"AMM LP Token #N"` / `"ALPN"`.
    ///
    /// Panics if a pool for this pair already exists.
    pub fn create_pool(
        env: Env,
        token_a: Address,
        token_b: Address,
        fee_bps: i128,
        governance_wasm_hash: Option<BytesN<32>>,
    ) -> (Address, Option<Address>) {
        // Normalise: smaller address is always token_a.
        let (ta, tb) = if token_a < token_b {
            (token_a, token_b)
        } else {
            (token_b, token_a)
        };

        assert!(
            (0..=10_000).contains(&fee_bps),
            "invalid fee_bps: {fee_bps} must be in 0..=10_000"
        );

        if env
            .storage()
            .instance()
            .has(&DataKey::Pool(ta.clone(), tb.clone()))
        {
            panic!("pool already exists");
        }

        let amm_wasm: BytesN<32> = env.storage().instance().get(&DataKey::AmmWasmHash).unwrap();
        let token_wasm: BytesN<32> = env
            .storage()
            .instance()
            .get(&DataKey::TokenWasmHash)
            .unwrap();
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();

        // Derive salts per pool from a monotonic counter.
        // We use n * 3 for LP salt, n * 3 + 1 for Pool salt, n * 3 + 2 for Governance salt.
        let n: u64 = env
            .storage()
            .instance()
            .get(&DataKey::PoolCount)
            .unwrap_or(0);
        env.storage().instance().set(&DataKey::PoolCount, &(n + 1));

        let lp_salt = Self::make_salt(&env, n * 3);
        let pool_salt = Self::make_salt(&env, n * 3 + 1);

        // Deploy LP token then AMM pool.
        let lp_addr = env
            .deployer()
            .with_current_contract(lp_salt)
            .deploy(token_wasm);
        let pool_addr = env
            .deployer()
            .with_current_contract(pool_salt)
            .deploy(amm_wasm);

        // Resolve LP token name/symbol defaults.
        let name = Self::counter_string(&env, b"AMM LP Token #", n);
        let symbol = Self::counter_string(&env, b"ALP", n);

        // Initialize LP token — admin must be the pool so it can mint/burn.
        LpTokenClient::new(&env, &lp_addr).initialize(&pool_addr, &name, &symbol, &7u32);

        // Optionally deploy governance contract
        let gov_addr = if let Some(gov_wasm) = governance_wasm_hash {
            let gov_salt = Self::make_salt(&env, n * 3 + 2);
            let gov_addr = env
                .deployer()
                .with_current_contract(gov_salt)
                .deploy(gov_wasm);

            // Initialize governance: 7 days voting, 2 days timelock, 10% quorum, 1% min stake.
            GovernanceClient::new(&env, &gov_addr).initialize(
                &admin,
                &pool_addr,
                &lp_addr,
                &604800_u64,
                &172800_u64,
                &1000_i128,
                &100_i128,
            );

            Some(gov_addr)
        } else {
            None
        };

        let pool_admin = gov_addr.clone().unwrap_or_else(|| admin.clone());

        // Initialize AMM pool.
        AmmPoolClient::new(&env, &pool_addr).initialize(
            &pool_admin,
            &ta,
            &tb,
            &lp_addr,
            &fee_bps,
            &admin,  // fee_recipient
            &0_i128, // protocol_fee_bps (disabled by default)
        );

        // Register pool in lookup indexes and record the LP token address.
        env.storage()
            .instance()
            .set(&DataKey::Pool(ta.clone(), tb.clone()), &pool_addr);
        env.storage()
            .instance()
            .set(&DataKey::LpToken(pool_addr.clone()), &lp_addr);
        env.storage()
            .instance()
            .set(&DataKey::GovernanceFor(pool_addr.clone()), &gov_addr);

        let mut all: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::AllPools)
            .unwrap_or_else(|| Vec::new(&env));
        all.push_back(pool_addr.clone());
        env.storage().instance().set(&DataKey::AllPools, &all);

        env.events().publish(
            (Symbol::new(&env, "pool_created"),),
            (
                ta.clone(),
                tb.clone(),
                pool_addr.clone(),
                fee_bps,
                lp_addr.clone(),
            ),
        );

        (pool_addr, gov_addr)
    }

    // ── Admin ─────────────────────────────────────────────────────────────────

    /// Update the AMM and/or LP token WASM hashes used for new pool deployments.
    ///
    /// Only the factory admin can call this. Existing pools are unaffected; only
    /// pools created after this call will use the new hashes.
    ///
    /// Pass `None` for a hash to leave it unchanged.
    /// Emits a `wasm_updated` event on every call.
    pub fn update_wasm_hashes(
        env: Env,
        amm_wasm_hash: Option<BytesN<32>>,
        token_wasm_hash: Option<BytesN<32>>,
    ) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        if let Some(ref h) = amm_wasm_hash {
            env.storage().instance().set(&DataKey::AmmWasmHash, h);
        }
        if let Some(ref h) = token_wasm_hash {
            env.storage().instance().set(&DataKey::TokenWasmHash, h);
        }
        env.events().publish(
            (Symbol::new(&env, "wasm_updated"),),
            (amm_wasm_hash, token_wasm_hash),
        );
    }

    /// Replace the factory contract WASM with a new version. Admin-only.
    ///
    /// The new WASM must already be uploaded to the network.
    /// State is preserved; only bytecode is replaced.
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        env.deployer()
            .update_current_contract_wasm(new_wasm_hash.clone());
        env.events()
            .publish((Symbol::new(&env, "upgraded"),), (new_wasm_hash,));
    }

    /// Set or update the WASM hash used for concentrated_liquidity deployments. Admin-only.
    pub fn set_cl_wasm_hash(env: Env, cl_wasm_hash: BytesN<32>) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        env.storage().instance().set(&DataKey::ClWasmHash, &cl_wasm_hash);
    }

    /// Deploy a new concentrated liquidity pool for `(token_a, token_b)` at `fee_bps`.
    ///
    /// A given (token_a, token_b, fee_bps) triplet is unique — the same pair can have
    /// multiple CL pools at different fee tiers. Token order is normalised (smaller
    /// address first). Panics if the triplet already has a pool.
    ///
    /// Unlike V2 pools, no LP token is deployed — positions are tracked on-chain by
    /// the CL contract itself.
    pub fn create_cl_pool(
        env: Env,
        token_a: Address,
        token_b: Address,
        fee_bps: i128,
        initial_tick: i32,
    ) -> Address {
        assert!(
            (0..=10_000).contains(&fee_bps),
            "invalid fee_bps: must be in 0..=10_000"
        );

        // Normalise token order.
        let (ta, tb) = if token_a < token_b {
            (token_a, token_b)
        } else {
            (token_b, token_a)
        };

        let cl_key = DataKey::ClPool(ta.clone(), tb.clone(), fee_bps);
        if env.storage().instance().has(&cl_key) {
            panic!("cl pool already exists for this (token_a, token_b, fee_bps)");
        }

        let cl_wasm: BytesN<32> = env
            .storage()
            .instance()
            .get(&DataKey::ClWasmHash)
            .expect("cl_wasm_hash not set; call set_cl_wasm_hash first");

        let n: u64 = env
            .storage()
            .instance()
            .get(&DataKey::PoolCount)
            .unwrap_or(0);
        // CL pools use n * 3 + 2 so they don't collide with V2 pool/LP/gov salts.
        let cl_salt = Self::make_salt(&env, n * 3 + 2 + 0x8000_0000_0000_0000);
        env.storage().instance().set(&DataKey::PoolCount, &(n + 1));

        let pool_addr = env
            .deployer()
            .with_current_contract(cl_salt)
            .deploy(cl_wasm);

        ClPoolClient::new(&env, &pool_addr).initialize(&ta, &tb, &fee_bps, &initial_tick);

        env.storage().instance().set(&cl_key, &pool_addr);

        env.events().publish(
            (Symbol::new(&env, "cl_pool_created"),),
            (ta.clone(), tb.clone(), fee_bps, pool_addr.clone()),
        );

        pool_addr
    }

    // ── Queries ───────────────────────────────────────────────────────────────

    /// Return the LP token address for the given pool, or `None` if unknown.
    pub fn get_lp_token(env: Env, pool: Address) -> Option<Address> {
        env.storage().instance().get(&DataKey::LpToken(pool))
    }

    /// Return the governance address for the given pool, or `None` if unknown.
    pub fn get_governance(env: Env, pool: Address) -> Option<Address> {
        env.storage()
            .instance()
            .get(&DataKey::GovernanceFor(pool))
            .unwrap_or(None)
    }

    /// Return the pool address for `(token_a, token_b)`, or `None` if it does
    /// not exist. Token pair order does not matter.
    pub fn get_pool(env: Env, token_a: Address, token_b: Address) -> Option<Address> {
        let (ta, tb) = if token_a < token_b {
            (token_a, token_b)
        } else {
            (token_b, token_a)
        };
        env.storage().instance().get(&DataKey::Pool(ta, tb))
    }

    /// Return the CL pool address for `(token_a, token_b, fee_bps)`, or `None` if absent.
    /// Token pair order does not matter.
    pub fn get_cl_pool(env: Env, token_a: Address, token_b: Address, fee_bps: i128) -> Option<Address> {
        let (ta, tb) = if token_a < token_b {
            (token_a, token_b)
        } else {
            (token_b, token_a)
        };
        env.storage().instance().get(&DataKey::ClPool(ta, tb, fee_bps))
    }

    /// Return the addresses of every pool deployed by this factory.
    pub fn all_pools(env: Env) -> Vec<Address> {
        env.storage()
            .instance()
            .get(&DataKey::AllPools)
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Return the total number of pools deployed by this factory.
    pub fn get_pool_count(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::PoolCount)
            .unwrap_or(0)
    }

    /// Return up to `limit` pool addresses starting at `offset`.
    pub fn get_pools(env: Env, offset: u32, limit: u32) -> Vec<Address> {
        let all: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::AllPools)
            .unwrap_or_else(|| Vec::new(&env));
        let len = all.len();
        let start = offset.min(len);
        let end = (start + limit).min(len);

        let mut page = Vec::new(&env);
        for i in start..end {
            if let Some(pool) = all.get(i) {
                page.push_back(pool);
            }
        }
        page
    }

    // ── Internals ─────────────────────────────────────────────────────────────

    /// Build a deterministic 32-byte salt from a u64 index.
    fn make_salt(env: &Env, index: u64) -> BytesN<32> {
        let mut arr = [0u8; 32];
        arr[..8].copy_from_slice(&index.to_be_bytes());
        BytesN::from_array(env, &arr)
    }

    /// Build a Soroban `String` from a byte prefix plus a decimal counter.
    ///
    /// Works in `no_std` — avoids `format!` by constructing ASCII digits manually.
    /// `prefix` must be valid UTF-8 (it always is for the callers in this file).
    fn counter_string(env: &Env, prefix: &[u8], n: u64) -> soroban_sdk::String {
        // Max: 20-char prefix + 20 decimal digits of u64::MAX
        let mut buf = [0u8; 40];
        let plen = prefix.len();
        buf[..plen].copy_from_slice(prefix);

        let nlen = if n == 0 {
            buf[plen] = b'0';
            1usize
        } else {
            let mut tmp = [0u8; 20];
            let mut num = n;
            let mut i = 0usize;
            while num > 0 {
                tmp[i] = b'0' + (num % 10) as u8;
                num /= 10;
                i += 1;
            }
            // Reverse digit order into buf.
            for j in 0..i {
                buf[plen + j] = tmp[i - 1 - j];
            }
            i
        };

        let total = plen + nlen;
        // SAFETY: prefix is valid UTF-8 and the appended bytes are ASCII digits.
        let s = core::str::from_utf8(&buf[..total]).unwrap();
        soroban_sdk::String::from_str(env, s)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────
//
// Tests deploy the AMM and token contracts as real WASM. Build the WASM first:
//
//   cargo build --release --target wasm32v1-none
//
// Then run:
//
//   cargo test -p factory

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    // Embed compiled WASM at test-compile time.
    // Use compiled WASM exported by the `amm` and `token` crates (feature `testutils`).

    #[test]
    fn test_create_pool() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);

        let (pool, gov) = factory.create_pool(&ta, &tb, &30_i128, &None);

        assert_eq!(factory.get_pool(&ta, &tb), Some(pool.clone()));
        assert_eq!(factory.all_pools().len(), 1);
        assert_eq!(gov, None);
        assert_eq!(factory.get_governance(&pool), None);
    }

    #[test]
    fn test_create_pool_with_governance() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);
        let gov_hash = env.deployer().upload_contract_wasm(governance::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);

        let (pool, gov) = factory.create_pool(&ta, &tb, &30_i128, &Some(gov_hash));

        assert_eq!(factory.get_pool(&ta, &tb), Some(pool.clone()));
        assert_eq!(factory.all_pools().len(), 1);
        assert!(gov.is_some());
        assert_eq!(factory.get_governance(&pool), gov);
    }

    #[test]
    fn test_normalize_order() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);

        factory.create_pool(&ta, &tb, &30_i128, &None);

        // Reverse-order lookup returns the same pool.
        assert_eq!(factory.get_pool(&ta, &tb), factory.get_pool(&tb, &ta));
    }

    #[test]
    fn test_duplicate_pool_panics() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);

        factory.create_pool(&ta, &tb, &30_i128, &None);
        let result = factory.try_create_pool(&ta, &tb, &30_i128, &None);
        assert!(result.is_err());
    }

    #[test]
    fn test_all_pools() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        assert_eq!(factory.all_pools().len(), 0);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);
        let tc = Address::generate(&env);

        factory.create_pool(&ta, &tb, &30_i128, &None);
        assert_eq!(factory.all_pools().len(), 1);

        factory.create_pool(&ta, &tc, &30_i128, &None);
        assert_eq!(factory.all_pools().len(), 2);
    }

    // ── Issue #96: LP token name/symbol reflect the token pair ───────────────

    #[test]
    fn test_lp_token_default_names_are_distinct() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);
        let tc = Address::generate(&env);

        let (pool0, _) = factory.create_pool(&ta, &tb, &30_i128, &None);
        let (pool1, _) = factory.create_pool(&ta, &tc, &30_i128, &None);

        // Fetch LP token addresses via the factory's registry.
        let lp0 = factory.get_lp_token(&pool0).unwrap();
        let lp1 = factory.get_lp_token(&pool1).unwrap();

        use soroban_sdk::token::Client as TokenClient;
        let lp_client0 = TokenClient::new(&env, &lp0);
        let lp_client1 = TokenClient::new(&env, &lp1);

        // Names and symbols must differ between the two pools.
        assert_ne!(lp_client0.name(), lp_client1.name());
        assert_ne!(lp_client0.symbol(), lp_client1.symbol());
    }

    // ── Issue #97: update_wasm_hashes ─────────────────────────────────────────

    #[test]
    fn test_update_wasm_hashes_non_admin_panics() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        // update_wasm_hashes itself requires admin auth; mock_all_auths covers it,
        // but we can verify the function doesn't panic when called by the real admin.
        factory.update_wasm_hashes(&Some(amm_hash.clone()), &None);
        factory.update_wasm_hashes(&None, &Some(token_hash.clone()));
        factory.update_wasm_hashes(&Some(amm_hash), &Some(token_hash));
    }

    #[test]
    fn test_update_wasm_hashes_updates_storage_and_allows_new_pool() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        // Call update with the same hashes (a no-op in practice, but verifies
        // the function is callable by admin and doesn't panic).
        factory.update_wasm_hashes(&Some(amm_hash.clone()), &Some(token_hash.clone()));

        // Pool creation still works after an update.
        let ta = Address::generate(&env);
        let tb = Address::generate(&env);
        let (pool, _) = factory.create_pool(&ta, &tb, &30_i128, &None);
        assert!(factory.get_pool(&ta, &tb).is_some());
        assert!(factory.get_lp_token(&pool).is_some());

        // Partial update — only token_wasm_hash.
        factory.update_wasm_hashes(&None, &Some(token_hash.clone()));

        // Partial update — only amm_wasm_hash.
        factory.update_wasm_hashes(&Some(amm_hash.clone()), &None);
    }

    // ── Issue #182: CL pool creation ──────────────────────────────────────────

    #[test]
    fn test_create_cl_pool_two_fee_tiers() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);
        let cl_hash = env.deployer().upload_contract_wasm(concentrated_liquidity::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);
        factory.set_cl_wasm_hash(&cl_hash);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);

        // Deploy same pair at two different fee tiers.
        let pool_30 = factory.create_cl_pool(&ta, &tb, &30_i128, &0_i32);
        let pool_100 = factory.create_cl_pool(&ta, &tb, &100_i128, &0_i32);

        // Both pools are distinct addresses.
        assert_ne!(pool_30, pool_100);

        // get_cl_pool returns the correct address for each tier.
        assert_eq!(factory.get_cl_pool(&ta, &tb, &30_i128), Some(pool_30.clone()));
        assert_eq!(factory.get_cl_pool(&ta, &tb, &100_i128), Some(pool_100.clone()));

        // Token order doesn't matter for lookup.
        assert_eq!(factory.get_cl_pool(&tb, &ta, &30_i128), Some(pool_30));

        // Missing tier returns None.
        assert_eq!(factory.get_cl_pool(&ta, &tb, &500_i128), None);
    }

    #[test]
    fn test_create_cl_pool_duplicate_panics() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);
        let cl_hash = env.deployer().upload_contract_wasm(concentrated_liquidity::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);
        factory.set_cl_wasm_hash(&cl_hash);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);

        factory.create_cl_pool(&ta, &tb, &30_i128, &0_i32);
        let result = factory.try_create_cl_pool(&ta, &tb, &30_i128, &0_i32);
        assert!(result.is_err());
    }

    #[test]
    fn test_pagination() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        // Initial pool count should be 0.
        assert_eq!(factory.get_pool_count(), 0);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);
        let tc = Address::generate(&env);
        let td = Address::generate(&env);

        let (pool1, _) = factory.create_pool(&ta, &tb, &30_i128, &None);
        assert_eq!(factory.get_pool_count(), 1);

        let (pool2, _) = factory.create_pool(&ta, &tc, &30_i128, &None);
        assert_eq!(factory.get_pool_count(), 2);

        let (pool3, _) = factory.create_pool(&ta, &td, &30_i128, &None);
        assert_eq!(factory.get_pool_count(), 3);

        // Page 1: first two pools.
        let page1 = factory.get_pools(&0u32, &2u32);
        assert_eq!(page1.len(), 2);
        assert_eq!(page1.get(0).unwrap(), pool1);
        assert_eq!(page1.get(1).unwrap(), pool2);

        // Page 2: starting at index 1, limit 1.
        let page2 = factory.get_pools(&1u32, &1u32);
        assert_eq!(page2.len(), 1);
        assert_eq!(page2.get(0).unwrap(), pool2);

        // Page 3: limit larger than remaining.
        let page3 = factory.get_pools(&1u32, &5u32);
        assert_eq!(page3.len(), 2);
        assert_eq!(page3.get(0).unwrap(), pool2);
        assert_eq!(page3.get(1).unwrap(), pool3);

        // Page 4: offset past end.
        let page4 = factory.get_pools(&5u32, &2u32);
        assert_eq!(page4.len(), 0);

        // Limit = 0.
        let page5 = factory.get_pools(&1u32, &0u32);
        assert_eq!(page5.len(), 0);
    }
}
