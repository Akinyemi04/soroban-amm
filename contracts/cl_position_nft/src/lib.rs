//! CL Position NFT – ERC-721-style receipt token for concentrated-liquidity positions.
//!
//! Each token represents an open CL position (`pool`, `lower_tick`, `upper_tick`).
//! Only the registered `cl_pool` address may mint or burn tokens; the pool calls
//! `mint` when a position opens and `burn` when it fully closes.
//!
//! Global state (admin, pool, id counter) lives in instance storage. Per-token
//! and per-owner state lives in persistent storage, matching the layout
//! established on `main`.
#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, Address, Env, Vec,
};

// ── WASM bytes for test harness ──────────────────────────────────────────────
#[cfg(feature = "testutils")]
pub const WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/wasm32v1-none/release/cl_position_nft.wasm"
));

// ── Errors ───────────────────────────────────────────────────────────────────
#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum NftError {
    AlreadyInitialized = 1,
    Unauthorized       = 2,
    TokenNotFound      = 3,
    NotOwnerOrApproved = 4,
    InvalidReceiver    = 5,
}

// ── Storage keys ─────────────────────────────────────────────────────────────
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum DataKey {
    /// Admin address, set once during `initialize`. Instance storage.
    Admin,
    /// Registered cl_pool contract, set once during `initialize`. Instance storage.
    ClPool,
    /// Monotonically-increasing counter; next token id to assign. Instance storage.
    NextTokenId,
    /// Owner of a token: `Owner(token_id) → Address`. Persistent.
    Owner(u64),
    /// Approved address for a single token: `Approved(token_id) → Address`. Persistent.
    Approved(u64),
    /// Operator approval over all of an owner's tokens:
    /// `OperatorApproval(owner, operator) → bool`. Persistent.
    OperatorApproval(Address, Address),
    /// Position metadata: `TokenPosition(token_id) → PositionMeta`. Persistent.
    TokenPosition(u64),
    /// All token ids owned by an address: `OwnedTokens(owner) → Vec<u64>`. Persistent.
    OwnedTokens(Address),
}

// ── Types ─────────────────────────────────────────────────────────────────────
/// Metadata attached to each NFT at mint-time.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PositionMeta {
    /// The CL pool contract that owns this position.
    pub pool:       Address,
    /// Lower tick of the position range.
    pub lower_tick: i32,
    /// Upper tick of the position range.
    pub upper_tick: i32,
}

// ── Contract ─────────────────────────────────────────────────────────────────
#[contract]
pub struct ClPositionNft;

#[contractimpl]
impl ClPositionNft {
    // ── One-time setup ────────────────────────────────────────────────────────

    /// Registers the admin and the CL pool address permitted to mint/burn.
    /// May only be called once.
    pub fn initialize(env: Env, admin: Address, cl_pool: Address) -> Result<(), NftError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(NftError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::ClPool, &cl_pool);
        env.storage().instance().set(&DataKey::NextTokenId, &0_u64);
        Ok(())
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    fn require_pool(env: &Env) -> Result<Address, NftError> {
        let pool: Address = env
            .storage()
            .instance()
            .get(&DataKey::ClPool)
            .ok_or(NftError::Unauthorized)?;
        pool.require_auth();
        Ok(pool)
    }

    // ── Core lifecycle ────────────────────────────────────────────────────────

    /// Mint a new position NFT. Callable **only** by the registered `cl_pool`.
    ///
    /// Increments `NextTokenId`, stores owner + position metadata, appends the
    /// token id to `OwnedTokens(to)`, and emits a `nft_mint` event.
    /// Returns the newly-assigned token id (sequential, starting at 0).
    pub fn mint(
        env: Env,
        to: Address,
        pool: Address,
        lower_tick: i32,
        upper_tick: i32,
    ) -> Result<u64, NftError> {
        Self::require_pool(&env)?;

        // Assign the next token id.
        let token_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextTokenId)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::NextTokenId, &(token_id + 1));

        // Store owner (persistent).
        env.storage()
            .persistent()
            .set(&DataKey::Owner(token_id), &to);

        // Store position metadata (persistent).
        let meta = PositionMeta {
            pool,
            lower_tick,
            upper_tick,
        };
        env.storage()
            .persistent()
            .set(&DataKey::TokenPosition(token_id), &meta);

        // Append to the owner's token list (persistent).
        let list_key = DataKey::OwnedTokens(to.clone());
        let mut owned: Vec<u64> = env
            .storage()
            .persistent()
            .get(&list_key)
            .unwrap_or_else(|| Vec::new(&env));
        owned.push_back(token_id);
        env.storage().persistent().set(&list_key, &owned);

        // Emit mint event: topic=(nft_mint, to), data=token_id.
        env.events()
            .publish((symbol_short!("nft_mint"), to), token_id);

        Ok(token_id)
    }

    /// Burn an existing position NFT. Callable **only** by the registered `cl_pool`.
    ///
    /// Removes `Owner`, `Approved`, and `TokenPosition`, prunes the id from
    /// `OwnedTokens(owner)`, and emits a `nft_burn` event.
    /// Returns [`NftError::TokenNotFound`] if the token does not exist.
    pub fn burn(env: Env, token_id: u64) -> Result<(), NftError> {
        Self::require_pool(&env)?;

        // Resolve the current owner – error if the token doesn't exist.
        let owner: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Owner(token_id))
            .ok_or(NftError::TokenNotFound)?;

        // Remove core token state.
        env.storage().persistent().remove(&DataKey::Owner(token_id));
        env.storage()
            .persistent()
            .remove(&DataKey::Approved(token_id));
        env.storage()
            .persistent()
            .remove(&DataKey::TokenPosition(token_id));

        // Remove from the owner's token list.
        let list_key = DataKey::OwnedTokens(owner.clone());
        let mut owned: Vec<u64> = env
            .storage()
            .persistent()
            .get(&list_key)
            .unwrap_or_else(|| Vec::new(&env));
        if let Some(idx) = owned.iter().position(|id| id == token_id) {
            owned.remove(idx as u32);
            env.storage().persistent().set(&list_key, &owned);
        }

        // Emit burn event: topic=(nft_burn, owner), data=token_id.
        env.events()
            .publish((symbol_short!("nft_burn"), owner), token_id);

        Ok(())
    }

    // ── View helpers ──────────────────────────────────────────────────────────

    /// Returns the owner of `token_id`, or [`NftError::TokenNotFound`].
    pub fn owner_of(env: Env, token_id: u64) -> Result<Address, NftError> {
        env.storage()
            .persistent()
            .get(&DataKey::Owner(token_id))
            .ok_or(NftError::TokenNotFound)
    }

    /// Returns the [`PositionMeta`] for `token_id`, or [`NftError::TokenNotFound`].
    pub fn position_meta(env: Env, token_id: u64) -> Result<PositionMeta, NftError> {
        env.storage()
            .persistent()
            .get(&DataKey::TokenPosition(token_id))
            .ok_or(NftError::TokenNotFound)
    }

    /// Returns all token ids owned by `owner` (empty vec if none).
    pub fn tokens_of(env: Env, owner: Address) -> Vec<u64> {
        env.storage()
            .persistent()
            .get(&DataKey::OwnedTokens(owner))
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Returns the number of tokens owned by `owner`.
    pub fn balance_of(env: Env, owner: Address) -> u32 {
        Self::tokens_of(env, owner).len()
    }

    /// Returns the total number of tokens ever minted (cumulative; not reduced by burns).
    pub fn total_supply(env: Env) -> u64 {
        Self::next_token_id(env)
    }

    /// Approve `approved` to transfer `token_id`. Callable by the token owner or an approved operator.
    pub fn approve(
        env: Env,
        caller: Address,
        approved: Address,
        token_id: u64,
    ) -> Result<(), NftError> {
        caller.require_auth();
        let owner: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Owner(token_id))
            .ok_or(NftError::TokenNotFound)?;

        let is_owner = caller == owner;
        let is_operator = Self::is_approved_for_all(env.clone(), owner.clone(), caller.clone());
        if !is_owner && !is_operator {
            return Err(NftError::Unauthorized);
        }

        env.storage()
            .persistent()
            .set(&DataKey::Approved(token_id), &approved);

        env.events()
            .publish((soroban_sdk::Symbol::new(&env, "approve"), caller, approved), token_id);

        Ok(())
    }

    /// Set operator approval for all tokens owned by `owner`.
    pub fn set_approval_for_all(
        env: Env,
        owner: Address,
        operator: Address,
        approved: bool,
    ) {
        owner.require_auth();
        env.storage()
            .persistent()
            .set(&DataKey::OperatorApproval(owner.clone(), operator.clone()), &approved);
        env.events()
            .publish((soroban_sdk::Symbol::new(&env, "approval_for_all"), owner, operator), approved);
    }

    /// Check if `operator` is approved for all tokens of `owner`.
    pub fn is_approved_for_all(env: Env, owner: Address, operator: Address) -> bool {
        env.storage()
            .persistent()
            .get(&DataKey::OperatorApproval(owner, operator))
            .unwrap_or(false)
    }

    /// Transfer `token_id` from `from` to `to`.
    /// Caller must be `from`, hold an approval for `token_id`, or be an approved operator for `from`.
    pub fn transfer(
        env: Env,
        caller: Address,
        from: Address,
        to: Address,
        token_id: u64,
    ) -> Result<(), NftError> {
        caller.require_auth();

        let owner: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Owner(token_id))
            .ok_or(NftError::TokenNotFound)?;

        if owner != from {
            return Err(NftError::Unauthorized);
        }

        let is_owner = caller == from;
        let is_approved = Self::get_approved(env.clone(), token_id).map(|a| a == caller).unwrap_or(false);
        let is_operator = Self::is_approved_for_all(env.clone(), from.clone(), caller.clone());

        if !is_owner && !is_approved && !is_operator {
            return Err(NftError::NotOwnerOrApproved);
        }

        // Update Owner
        env.storage().persistent().set(&DataKey::Owner(token_id), &to);

        // Clear Approved
        env.storage().persistent().remove(&DataKey::Approved(token_id));

        // Update from OwnedTokens
        let from_key = DataKey::OwnedTokens(from.clone());
        let mut from_owned: Vec<u64> = env
            .storage()
            .persistent()
            .get(&from_key)
            .unwrap_or_else(|| Vec::new(&env));
        if let Some(idx) = from_owned.iter().position(|id| id == token_id) {
            from_owned.remove(idx as u32);
            env.storage().persistent().set(&from_key, &from_owned);
        }

        // Update to OwnedTokens
        let to_key = DataKey::OwnedTokens(to.clone());
        let mut to_owned: Vec<u64> = env
            .storage()
            .persistent()
            .get(&to_key)
            .unwrap_or_else(|| Vec::new(&env));
        to_owned.push_back(token_id);
        env.storage().persistent().set(&to_key, &to_owned);

        // Emit transfer event
        env.events()
            .publish((soroban_sdk::Symbol::new(&env, "transfer"), from, to), token_id);

        Ok(())
    }

    /// Returns the currently-approved address for `token_id`, if any.
    pub fn get_approved(env: Env, token_id: u64) -> Option<Address> {
        env.storage().persistent().get(&DataKey::Approved(token_id))
    }

    /// Returns the registered admin address.
    pub fn admin(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Admin).unwrap()
    }

    /// Returns the registered `cl_pool` address.
    pub fn cl_pool(env: Env) -> Address {
        env.storage().instance().get(&DataKey::ClPool).unwrap()
    }

    /// Returns the next token id that will be assigned.
    pub fn next_token_id(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::NextTokenId)
            .unwrap_or(0)
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    /// Returns (env, client, admin, pool, user) with the contract initialized
    /// and all auths mocked.
    fn setup() -> (Env, ClPositionNftClient<'static>, Address, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(ClPositionNft, ());
        let client = ClPositionNftClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let pool = Address::generate(&env);
        let user = Address::generate(&env);
        client.initialize(&admin, &pool);
        (env, client, admin, pool, user)
    }

    // ── initialize ─────────────────────────────────────────────────────────────

    #[test]
    fn initialize_stores_global_state() {
        let (_, client, admin, pool, _) = setup();
        assert_eq!(client.admin(), admin);
        assert_eq!(client.cl_pool(), pool);
        assert_eq!(client.next_token_id(), 0);
    }

    #[test]
    fn initialize_twice_returns_already_initialized() {
        let (env, client, _admin, pool, _) = setup();
        let other_admin = Address::generate(&env);
        let err = client
            .try_initialize(&other_admin, &pool)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, NftError::AlreadyInitialized);
    }

    // ── mint ─────────────────────────────────────────────────────────────────

    #[test]
    fn mint_assigns_sequential_ids_starting_at_zero() {
        let (env, client, _admin, pool, user) = setup();

        let id0 = client.mint(&user, &pool, &-100, &100);
        let id1 = client.mint(&user, &pool, &-200, &200);

        assert_eq!(id0, 0);
        assert_eq!(id1, 1);

        assert_eq!(client.owner_of(&id0), user);
        assert_eq!(client.owner_of(&id1), user);

        let meta0 = client.position_meta(&id0);
        assert_eq!(meta0.lower_tick, -100);
        assert_eq!(meta0.upper_tick, 100);

        let owned = client.tokens_of(&user);
        assert_eq!(owned.len(), 2);
        assert_eq!(owned.get(0), Some(0_u64));
        assert_eq!(owned.get(1), Some(1_u64));

        // Events are published; the harness captures them.
        let _ = env.events().all();
    }

    #[test]
    fn mint_stores_correct_position_meta() {
        let (_, client, _admin, pool, user) = setup();
        let id = client.mint(&user, &pool, &-500, &500);
        let meta = client.position_meta(&id);
        assert_eq!(meta.pool, pool);
        assert_eq!(meta.lower_tick, -500);
        assert_eq!(meta.upper_tick, 500);
    }

    // ── burn ─────────────────────────────────────────────────────────────────

    #[test]
    fn burn_clears_all_state() {
        let (env, client, _admin, pool, user) = setup();

        // Mint then set an approval to verify it is also cleared.
        let id = client.mint(&user, &pool, &-100, &100);
        let approved_addr = Address::generate(&env);
        client.approve(&user, &approved_addr, &id);
        assert_eq!(client.get_approved(&id), Some(approved_addr));

        client.burn(&id);

        assert!(client.try_owner_of(&id).is_err());
        assert!(client.try_position_meta(&id).is_err());
        assert_eq!(client.get_approved(&id), None);
        assert_eq!(client.tokens_of(&user).len(), 0);
    }

    #[test]
    fn double_burn_returns_token_not_found() {
        let (_, client, _admin, pool, user) = setup();
        let id = client.mint(&user, &pool, &-100, &100);
        client.burn(&id);
        let err = client.try_burn(&id).unwrap_err().unwrap();
        assert_eq!(err, NftError::TokenNotFound);
    }

    #[test]
    fn burn_non_existent_token_returns_token_not_found() {
        let (_, client, _admin, _pool, _) = setup();
        let err = client.try_burn(&999_u64).unwrap_err().unwrap();
        assert_eq!(err, NftError::TokenNotFound);
    }

    // ── authorization ────────────────────────────────────────────────────────

    #[test]
    #[should_panic]
    fn mint_requires_pool_auth() {
        let env = Env::default();
        let contract_id = env.register(ClPositionNft, ());
        let client = ClPositionNftClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let pool = Address::generate(&env);
        let user = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &pool);

        // No auths for the next call: pool.require_auth() must fail.
        env.set_auths(&[]);
        client.mint(&user, &pool, &-100, &100);
    }

    #[test]
    #[should_panic]
    fn burn_requires_pool_auth() {
        let env = Env::default();
        let contract_id = env.register(ClPositionNft, ());
        let client = ClPositionNftClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let pool = Address::generate(&env);
        let user = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &pool);
        let id = client.mint(&user, &pool, &-100, &100);

        // Strip auth, then burn: pool.require_auth() must fail.
        env.set_auths(&[]);
        client.burn(&id);
    }

    // ── view helpers ─────────────────────────────────────────────────────────

    #[test]
    fn owner_of_non_existent_token_returns_not_found() {
        let (_, client, _admin, _pool, _) = setup();
        let err = client.try_owner_of(&42_u64).unwrap_err().unwrap();
        assert_eq!(err, NftError::TokenNotFound);
    }

    #[test]
    fn tokens_of_empty_returns_empty_vec() {
        let (env, client, _admin, _pool, _) = setup();
        let nobody = Address::generate(&env);
        assert_eq!(client.tokens_of(&nobody).len(), 0);
    }

    #[test]
    fn multiple_users_have_independent_token_lists() {
        let (env, client, _admin, pool, user_a) = setup();
        let user_b = Address::generate(&env);

        let id0 = client.mint(&user_a, &pool, &-100, &100);
        let id1 = client.mint(&user_b, &pool, &-200, &200);
        let id2 = client.mint(&user_a, &pool, &-300, &300);

        let a_owned = client.tokens_of(&user_a);
        let b_owned = client.tokens_of(&user_b);

        assert_eq!(a_owned.len(), 2);
        assert!(a_owned.iter().any(|id| id == id0));
        assert!(a_owned.iter().any(|id| id == id2));

        assert_eq!(b_owned.len(), 1);
        assert!(b_owned.iter().any(|id| id == id1));
    }

    // ── balance_of / total_supply ──────────────────────────────────────────────

    #[test]
    fn balance_of_tracks_mint_and_burn() {
        let (_env, client, _admin, pool, user) = setup();
        assert_eq!(client.balance_of(&user), 0);

        let id = client.mint(&user, &pool, &-100, &100);
        assert_eq!(client.balance_of(&user), 1);

        client.burn(&id);
        assert_eq!(client.balance_of(&user), 0);
    }

    #[test]
    fn total_supply_tracks_all_mints() {
        let (_env, client, _admin, pool, user) = setup();
        assert_eq!(client.total_supply(), 0);

        let id0 = client.mint(&user, &pool, &-100, &100);
        assert_eq!(client.total_supply(), 1);

        client.mint(&user, &pool, &-200, &200);
        assert_eq!(client.total_supply(), 2);

        client.burn(&id0);
        // Burning does not decrease total_supply.
        assert_eq!(client.total_supply(), 2);
    }

    // ── transfer and approval ──────────────────────────────────────────────────

    #[test]
    fn transfer_happy_path() {
        let (env, client, _admin, pool, user_a) = setup();
        let user_b = Address::generate(&env);
        let id = client.mint(&user_a, &pool, &-100, &100);

        client.transfer(&user_a, &user_a, &user_b, &id);

        assert_eq!(client.owner_of(&id), user_b);
        assert_eq!(client.tokens_of(&user_a).len(), 0);
        let b_tokens = client.tokens_of(&user_b);
        assert_eq!(b_tokens.len(), 1);
        assert_eq!(b_tokens.get(0).unwrap(), id);
    }

    #[test]
    fn approve_then_transfer_clears_approval() {
        let (env, client, _admin, pool, user_a) = setup();
        let operator = Address::generate(&env);
        let user_b = Address::generate(&env);
        let id = client.mint(&user_a, &pool, &-100, &100);

        client.approve(&user_a, &operator, &id);
        assert_eq!(client.get_approved(&id), Some(operator.clone()));

        client.transfer(&operator, &user_a, &user_b, &id);

        assert_eq!(client.owner_of(&id), user_b);
        assert_eq!(client.get_approved(&id), None); // Approval must be cleared
    }

    #[test]
    fn operator_can_transfer() {
        let (env, client, _admin, pool, user_a) = setup();
        let operator = Address::generate(&env);
        let user_b = Address::generate(&env);
        let id = client.mint(&user_a, &pool, &-100, &100);

        client.set_approval_for_all(&user_a, &operator, &true);
        assert_eq!(client.is_approved_for_all(&user_a, &operator), true);

        client.transfer(&operator, &user_a, &user_b, &id);

        assert_eq!(client.owner_of(&id), user_b);
    }

    #[test]
    fn unauthorized_transfer_fails() {
        let (env, client, _admin, pool, user_a) = setup();
        let unauthorized = Address::generate(&env);
        let user_b = Address::generate(&env);
        let id = client.mint(&user_a, &pool, &-100, &100);

        let res = client.try_transfer(&unauthorized, &user_a, &user_b, &id);
        assert_eq!(res.unwrap_err().unwrap(), NftError::NotOwnerOrApproved);
    }

    #[test]
    fn transfer_from_wrong_owner_fails() {
        let (env, client, _admin, pool, user_a) = setup();
        let user_b = Address::generate(&env);
        let id = client.mint(&user_a, &pool, &-100, &100);

        let res = client.try_transfer(&user_a, &user_b, &user_b, &id);
        assert_eq!(res.unwrap_err().unwrap(), NftError::Unauthorized);
    }
}