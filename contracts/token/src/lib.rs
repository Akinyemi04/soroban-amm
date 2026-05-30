//! SEP-41 compliant fungible token contract used as the LP token for the AMM.

#![no_std]

use soroban_sdk::{contract, contractimpl, contracttype, Address, BytesN, Env, String, Symbol, Vec};

// Export compiled WASM for tests/dev usage when the `testutils` feature is enabled.
// When tests run, the WASM file may not exist; use empty slice to avoid error.
#[cfg(feature = "testutils")]
pub const WASM: &[u8] = &[];

#[contracttype]
pub enum DataKey {
    Balance(Address),
    Locked(Address),
    Allowance(Address, Address),
    Admin,
    Locker,
    Name,
    Symbol,
    Decimals,
    TotalSupply,
    /// Historical balance checkpoints for governance snapshots.
    Checkpoints(Address),
}

/// Balance recorded at a ledger sequence (used by `balance_at`).
#[contracttype]
#[derive(Clone, Debug)]
pub struct Checkpoint {
    pub ledger: u32,
    pub balance: i128,
}

#[contract]
pub struct LpToken;

#[contractimpl]
impl LpToken {
    /// Initialize the token with metadata and an admin that can mint/burn.
    ///
    /// `admin` is the only address authorized to call `mint` and `burn`.
    /// Panics if the contract has already been initialized.
    pub fn initialize(env: Env, admin: Address, name: String, symbol: String, decimals: u32) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!(
                "already initialized: contract {:?}",
                env.current_contract_address()
            );
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Locker, &admin);
        env.storage().instance().set(&DataKey::Name, &name);
        env.storage().instance().set(&DataKey::Symbol, &symbol);
        env.storage().instance().set(&DataKey::Decimals, &decimals);
        env.storage().instance().set(&DataKey::TotalSupply, &0_i128);
    }

    // ── Read ──────────────────────────────────────────────────────────────────

    /// Returns the token name.
    pub fn name(env: Env) -> String {
        env.storage().instance().get(&DataKey::Name).unwrap()
    }

    /// Returns the token symbol.
    pub fn symbol(env: Env) -> String {
        env.storage().instance().get(&DataKey::Symbol).unwrap()
    }

    /// Returns the number of decimal places used to represent token amounts.
    pub fn decimals(env: Env) -> u32 {
        env.storage().instance().get(&DataKey::Decimals).unwrap()
    }

    /// Returns the total number of tokens currently in circulation.
    pub fn total_supply(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalSupply)
            .unwrap_or(0)
    }

    /// Returns the token balance of `id`. Returns `0` if the account has no balance.
    pub fn balance(env: Env, id: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Balance(id))
            .unwrap_or(0)
    }

    /// Returns the balance of `id` at or before `ledger` (for governance snapshots).
    pub fn balance_at(env: Env, id: Address, ledger: u32) -> i128 {
        let checkpoints: Vec<Checkpoint> = env
            .storage()
            .persistent()
            .get(&DataKey::Checkpoints(id))
            .unwrap_or(Vec::new(&env));
        let mut result = 0_i128;
        for i in 0..checkpoints.len() {
            let cp = checkpoints.get(i).unwrap();
            if cp.ledger <= ledger {
                result = cp.balance;
            } else {
                break;
            }
        }
        result
    }

    /// Returns the amount `spender` is allowed to transfer on behalf of `from`.
    /// Returns `0` if no allowance has been set.
    pub fn allowance(env: Env, from: Address, spender: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Allowance(from, spender))
            .unwrap_or(0)
    }

    // ── Write ─────────────────────────────────────────────────────────────────

    /// Transfer `amount` tokens from `from` to `to`.
    ///
    /// Requires authorization from `from`.
    /// Panics if `from` has insufficient balance.
    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
        from.require_auth();
        Self::_transfer(&env, &from, &to, amount);
    }

    /// Transfer `amount` tokens from `from` to `to` using a pre-approved allowance.
    ///
    /// Requires authorization from `spender`.
    /// Panics if the current allowance of `spender` over `from` is less than `amount`.
    /// Panics if `from` has insufficient balance.
    pub fn transfer_from(env: Env, spender: Address, from: Address, to: Address, amount: i128) {
        spender.require_auth();
        let allowance = Self::allowance(env.clone(), from.clone(), spender.clone());
        assert!(
            allowance >= amount,
            "insufficient allowance: available={allowance}, requested={amount}"
        );
        env.storage().persistent().set(
            &DataKey::Allowance(from.clone(), spender),
            &(allowance - amount),
        );
        Self::_transfer(&env, &from, &to, amount);
    }

    /// Approve `spender` to transfer up to `amount` tokens on behalf of `from`.
    ///
    /// Requires authorization from `from`.
    /// Setting `amount` to `0` effectively revokes the allowance.
    pub fn approve(env: Env, from: Address, spender: Address, amount: i128) {
        from.require_auth();
        env.storage()
            .persistent()
            .set(&DataKey::Allowance(from, spender), &amount);
    }

    /// Mint new tokens — admin only (called by the AMM contract).
    pub fn mint(env: Env, to: Address, amount: i128) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        let supply: i128 = Self::total_supply(env.clone());
        env.storage()
            .instance()
            .set(&DataKey::TotalSupply, &(supply + amount));
        let bal = Self::balance(env.clone(), to.clone());
        env.storage()
            .persistent()
            .set(&DataKey::Balance(to.clone()), &(bal + amount));
        Self::write_checkpoint(&env, &to);
    }

    /// Burn tokens — admin only (called by the AMM contract).
    pub fn burn(env: Env, from: Address, amount: i128) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        let bal = Self::balance(env.clone(), from.clone());
        assert!(
            bal >= amount,
            "insufficient balance: available={bal}, requested={amount}"
        );
        env.storage()
            .persistent()
            .set(&DataKey::Balance(from.clone()), &(bal - amount));
        let supply: i128 = Self::total_supply(env.clone());
        env.storage()
            .instance()
            .set(&DataKey::TotalSupply, &(supply - amount));
        Self::write_checkpoint(&env, &from);
    }

    // ── Internal ──────────────────────────────────────────────────────────────

    /// Returns the admin address that is authorized to mint and burn tokens.
    pub fn admin(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Admin).unwrap()
    }

    /// Address allowed to lock/unlock balances (governance).
    pub fn locker(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Locker).unwrap()
    }

    /// Replace the contract WASM with a new version. Admin-only.
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

    /// Admin-only locker update.
    pub fn set_locker(env: Env, locker: Address) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        env.storage().instance().set(&DataKey::Locker, &locker);
    }

    /// Returns currently locked balance for `id`.
    pub fn locked_balance(env: Env, id: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Locked(id))
            .unwrap_or(0)
    }

    /// Governance-locker only: lock a holder's transferable balance.
    pub fn lock(env: Env, holder: Address, amount: i128) {
        assert!(amount > 0, "amount must be positive");
        let locker: Address = env.storage().instance().get(&DataKey::Locker).unwrap();
        locker.require_auth();
        let bal = Self::balance(env.clone(), holder.clone());
        let locked = Self::locked_balance(env.clone(), holder.clone());
        assert!(
            bal - locked >= amount,
            "insufficient unlocked balance to lock"
        );
        env.storage()
            .persistent()
            .set(&DataKey::Locked(holder), &(locked + amount));
    }

    /// Governance-locker only: unlock previously locked balance.
    pub fn unlock(env: Env, holder: Address, amount: i128) {
        assert!(amount > 0, "amount must be positive");
        let locker: Address = env.storage().instance().get(&DataKey::Locker).unwrap();
        locker.require_auth();
        let locked = Self::locked_balance(env.clone(), holder.clone());
        assert!(locked >= amount, "unlock exceeds locked balance");
        env.storage()
            .persistent()
            .set(&DataKey::Locked(holder), &(locked - amount));
    }

    fn _transfer(env: &Env, from: &Address, to: &Address, amount: i128) {
        let from_bal = Self::balance(env.clone(), from.clone());
        let locked = Self::locked_balance(env.clone(), from.clone());
        assert!(
            from_bal - locked >= amount,
            "insufficient unlocked balance: available={}, requested={amount}",
            from_bal - locked
        );
        env.storage()
            .persistent()
            .set(&DataKey::Balance(from.clone()), &(from_bal - amount));
        let to_bal = Self::balance(env.clone(), to.clone());
        env.storage()
            .persistent()
            .set(&DataKey::Balance(to.clone()), &(to_bal + amount));
        Self::write_checkpoint(env, from);
        Self::write_checkpoint(env, to);
        env.events().publish(
            (Symbol::new(env, "transfer"), from.clone()),
            (to.clone(), amount),
        );
    }

    fn write_checkpoint(env: &Env, account: &Address) {
        let ledger = env.ledger().sequence();
        let balance = Self::balance(env.clone(), account.clone());
        let key = DataKey::Checkpoints(account.clone());
        let mut checkpoints: Vec<Checkpoint> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or(Vec::new(env));
        if checkpoints.len() > 0 {
            let last = checkpoints.get(checkpoints.len() - 1).unwrap();
            if last.ledger == ledger {
                checkpoints.pop_back();
            }
        }
        checkpoints.push_back(Checkpoint { ledger, balance });
        env.storage().persistent().set(&key, &checkpoints);
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    struct TestSetup {
        env: Env,
        admin: Address,
        contract_addr: Address,
    }

    fn setup() -> TestSetup {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let contract_addr = env.register_contract(None, LpToken);
        LpTokenClient::new(&env, &contract_addr).initialize(
            &admin,
            &String::from_str(&env, "Test Token"),
            &String::from_str(&env, "TST"),
            &7u32,
        );
        TestSetup {
            env,
            admin,
            contract_addr,
        }
    }

    #[test]
    fn test_initialize_twice_panics() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let result = client.try_initialize(
            &ts.admin,
            &String::from_str(&ts.env, "X"),
            &String::from_str(&ts.env, "X"),
            &7u32,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_mint_and_burn() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let user = Address::generate(&ts.env);

        client.mint(&user, &1_000_i128);
        assert_eq!(client.balance(&user), 1_000);
        assert_eq!(client.total_supply(), 1_000);

        client.burn(&user, &400_i128);
        assert_eq!(client.balance(&user), 600);
        assert_eq!(client.total_supply(), 600);
    }

    #[test]
    fn test_burn_insufficient_balance_panics() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let user = Address::generate(&ts.env);
        client.mint(&user, &100_i128);
        let result = client.try_burn(&user, &200_i128);
        assert!(result.is_err());
    }

    #[test]
    fn test_transfer() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let bob = Address::generate(&ts.env);

        client.mint(&alice, &500_i128);
        client.transfer(&alice, &bob, &200_i128);

        assert_eq!(client.balance(&alice), 300);
        assert_eq!(client.balance(&bob), 200);
        assert_eq!(client.total_supply(), 500);
    }

    #[test]
    fn test_transfer_insufficient_balance_panics() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let bob = Address::generate(&ts.env);
        client.mint(&alice, &100_i128);
        let result = client.try_transfer(&alice, &bob, &200_i128);
        assert!(result.is_err());
    }

    #[test]
    fn test_approve_and_transfer_from() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let bob = Address::generate(&ts.env);
        let carol = Address::generate(&ts.env);

        client.mint(&alice, &1_000_i128);
        client.approve(&alice, &bob, &300_i128);
        assert_eq!(client.allowance(&alice, &bob), 300);

        client.transfer_from(&bob, &alice, &carol, &200_i128);
        assert_eq!(client.balance(&alice), 800);
        assert_eq!(client.balance(&carol), 200);
        assert_eq!(client.allowance(&alice, &bob), 100);
    }

    #[test]
    fn test_transfer_from_insufficient_allowance_panics() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let bob = Address::generate(&ts.env);
        let carol = Address::generate(&ts.env);

        client.mint(&alice, &1_000_i128);
        client.approve(&alice, &bob, &50_i128);
        let result = client.try_transfer_from(&bob, &alice, &carol, &100_i128);
        assert!(result.is_err());
    }

    #[test]
    fn test_metadata() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        assert_eq!(client.name(), String::from_str(&ts.env, "Test Token"));
        assert_eq!(client.symbol(), String::from_str(&ts.env, "TST"));
        assert_eq!(client.decimals(), 7u32);
    }

    #[test]
    fn test_balance_at_snapshot() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let bob = Address::generate(&ts.env);

        client.mint(&alice, &1_000_i128);
        let ledger_after_mint = ts.env.ledger().sequence();
        client.transfer(&alice, &bob, &400_i128);

        assert_eq!(client.balance_at(&alice, &ledger_after_mint), 1_000);
        assert_eq!(client.balance(&alice), 600);
    }

    #[test]
    fn test_lock_blocks_transfer_until_unlock() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let bob = Address::generate(&ts.env);
        let locker = Address::generate(&ts.env);

        client.set_locker(&locker);
        client.mint(&alice, &1_000_i128);
        client.lock(&alice, &700_i128);
        assert_eq!(client.locked_balance(&alice), 700);

        assert!(client.try_transfer(&alice, &bob, &400_i128).is_err());
        client.transfer(&alice, &bob, &300_i128);

        client.unlock(&alice, &700_i128);
        assert_eq!(client.locked_balance(&alice), 0);
        client.transfer(&alice, &bob, &700_i128);
        assert_eq!(client.balance(&alice), 0);
        assert_eq!(client.balance(&bob), 1_000);
    }
}
