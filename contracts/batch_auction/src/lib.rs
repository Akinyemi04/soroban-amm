//! MEV-resistant batch auction contract.
//!
//! Collects swap orders within a configurable time window and settles them
//! atomically in a single transaction. Because no external trade can be
//! inserted between batched orders during settlement, sandwich attacks are
//! structurally impossible for orders in the same batch window.
//!
//! Flow:
//!   1. Deploy and `initialize` with an admin and a batch window duration.
//!   2. Traders call `submit_order` — tokens are escrowed immediately.
//!   3. After the window elapses, anyone calls `settle_batch`.
//!   4. Settlement executes all orders atomically; output tokens go to traders.
//!   5. Any trader may call `cancel_order` before settlement to reclaim tokens.

#![no_std]

use soroban_sdk::{
    contract, contractimpl, contracttype, contracterror, symbol_short, Address, Env, Symbol, Vec,
};
use soroban_sdk::token::Client as SepTokenClient;
use amm::AmmPoolClient;

// ── Errors ────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum AuctionError {
    AlreadyInitialized = 1,
    Unauthorized       = 2,
    OrderNotFound      = 3,
    BatchWindowOpen    = 4,
    NoOrders           = 5,
    ZeroAmount         = 6,
    DeadlineExceeded   = 7,
}

// ── Storage types ─────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, Debug)]
pub struct Order {
    pub id: u64,
    pub trader: Address,
    pub pool: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: i128,
    pub min_out: i128,
    pub submitted_at: u64,
}

#[contracttype]
pub enum DataKey {
    Admin,
    BatchWindowSecs,
    BatchOpenedAt,
    NextOrderId,
    Order(u64),
    PendingOrders,
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct BatchAuction;

#[contractimpl]
impl BatchAuction {
    /// Initialize the auction contract.
    ///
    /// - `batch_window_secs` — how long (in ledger seconds) a batch window stays
    ///   open before it can be settled.
    pub fn initialize(
        env: Env,
        admin: Address,
        batch_window_secs: u64,
    ) -> Result<(), AuctionError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(AuctionError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::BatchWindowSecs, &batch_window_secs);
        env.storage()
            .instance()
            .set(&DataKey::NextOrderId, &0_u64);
        env.storage()
            .instance()
            .set(&DataKey::PendingOrders, &Vec::<u64>::new(&env));
        env.storage()
            .instance()
            .set(&DataKey::BatchOpenedAt, &env.ledger().timestamp());
        Ok(())
    }

    /// Submit a swap order and escrow `amount_in` of `token_in`.
    ///
    /// Tokens are pulled from `trader` immediately so the batch holds a firm
    /// commitment. `token_out` must be the other pool token (not validated
    /// on-chain — mismatches are caught at settlement time).
    ///
    /// Returns the new order ID.
    pub fn submit_order(
        env: Env,
        trader: Address,
        pool: Address,
        token_in: Address,
        token_out: Address,
        amount_in: i128,
        min_out: i128,
        deadline: u64,
    ) -> Result<u64, AuctionError> {
        if deadline < env.ledger().timestamp() {
            return Err(AuctionError::DeadlineExceeded);
        }
        if amount_in <= 0 {
            return Err(AuctionError::ZeroAmount);
        }
        trader.require_auth();

        // Escrow input tokens immediately so the commitment is firm.
        SepTokenClient::new(&env, &token_in).transfer(
            &trader,
            &env.current_contract_address(),
            &amount_in,
        );

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextOrderId)
            .unwrap_or(0);

        let order = Order {
            id,
            trader: trader.clone(),
            pool,
            token_in,
            token_out,
            amount_in,
            min_out,
            submitted_at: env.ledger().timestamp(),
        };

        env.storage().instance().set(&DataKey::Order(id), &order);

        let mut pending: Vec<u64> = env
            .storage()
            .instance()
            .get(&DataKey::PendingOrders)
            .unwrap_or_else(|| Vec::new(&env));
        pending.push_back(id);
        env.storage()
            .instance()
            .set(&DataKey::PendingOrders, &pending);
        env.storage()
            .instance()
            .set(&DataKey::NextOrderId, &(id + 1));

        env.events().publish(
            (Symbol::new(&env, "order_submitted"), trader),
            (id, amount_in),
        );

        Ok(id)
    }

    /// Cancel a pending order and refund escrowed tokens.
    ///
    /// Only the original trader may cancel their own order.
    pub fn cancel_order(
        env: Env,
        trader: Address,
        order_id: u64,
    ) -> Result<(), AuctionError> {
        trader.require_auth();

        let order: Order = env
            .storage()
            .instance()
            .get(&DataKey::Order(order_id))
            .ok_or(AuctionError::OrderNotFound)?;

        if order.trader != trader {
            return Err(AuctionError::Unauthorized);
        }

        // Refund escrowed tokens.
        SepTokenClient::new(&env, &order.token_in).transfer(
            &env.current_contract_address(),
            &trader,
            &order.amount_in,
        );

        env.storage().instance().remove(&DataKey::Order(order_id));

        let mut pending: Vec<u64> = env
            .storage()
            .instance()
            .get(&DataKey::PendingOrders)
            .unwrap_or_else(|| Vec::new(&env));
        let mut updated = Vec::<u64>::new(&env);
        for i in 0..pending.len() {
            let oid = pending.get(i).unwrap();
            if oid != order_id {
                updated.push_back(oid);
            }
        }
        env.storage()
            .instance()
            .set(&DataKey::PendingOrders, &updated);

        env.events().publish(
            (Symbol::new(&env, "order_cancelled"), trader),
            (order_id,),
        );

        Ok(())
    }

    /// Settle the current batch atomically.
    ///
    /// Callable by anyone once the batch window has elapsed. All pending orders
    /// execute sequentially within a single transaction — no external trade can
    /// be inserted between them, which structurally prevents sandwich attacks.
    ///
    /// If any individual swap fails (e.g. `min_out` not met), the entire
    /// settlement reverts and escrowed funds are automatically returned by the
    /// Soroban runtime.
    ///
    /// Returns the output amounts for each order in submission order.
    pub fn settle_batch(env: Env) -> Result<Vec<i128>, AuctionError> {
        let opened_at: u64 = env
            .storage()
            .instance()
            .get(&DataKey::BatchOpenedAt)
            .unwrap_or(0);
        let window_secs: u64 = env
            .storage()
            .instance()
            .get(&DataKey::BatchWindowSecs)
            .unwrap_or(60);
        let now = env.ledger().timestamp();
        if now < opened_at + window_secs {
            return Err(AuctionError::BatchWindowOpen);
        }

        let pending: Vec<u64> = env
            .storage()
            .instance()
            .get(&DataKey::PendingOrders)
            .unwrap_or_else(|| Vec::new(&env));
        if pending.is_empty() {
            return Err(AuctionError::NoOrders);
        }

        let auction_addr = env.current_contract_address();
        let settlement_deadline = now + window_secs;
        let mut results = Vec::<i128>::new(&env);

        for i in 0..pending.len() {
            let order_id = pending.get(i).unwrap();
            let order: Order = env
                .storage()
                .instance()
                .get(&DataKey::Order(order_id))
                .unwrap();

            // Execute the swap on behalf of the batch auction contract.
            // Authorization for the token pull (auction → pool) is automatically
            // satisfied because the batch_auction is the invoking contract.
            let amount_out = AmmPoolClient::new(&env, &order.pool)
                .swap(
                    &auction_addr,
                    &order.token_in,
                    &order.amount_in,
                    &order.min_out,
                    &settlement_deadline,
                    &None::<Address>,
                )
                .unwrap_or_else(|_| panic!("order {} swap failed", order_id));

            // Forward output tokens to the original trader.
            SepTokenClient::new(&env, &order.token_out).transfer(
                &auction_addr,
                &order.trader,
                &amount_out,
            );

            results.push_back(amount_out);
            env.storage().instance().remove(&DataKey::Order(order_id));
        }

        // Reset for the next batch window.
        env.storage()
            .instance()
            .set(&DataKey::PendingOrders, &Vec::<u64>::new(&env));
        env.storage()
            .instance()
            .set(&DataKey::BatchOpenedAt, &now);

        env.events().publish(
            (symbol_short!("settled"),),
            (pending.len() as u32,),
        );

        Ok(results)
    }

    /// Return all pending orders in the current batch window.
    pub fn get_pending_orders(env: Env) -> Vec<Order> {
        let pending: Vec<u64> = env
            .storage()
            .instance()
            .get(&DataKey::PendingOrders)
            .unwrap_or_else(|| Vec::new(&env));
        let mut orders = Vec::<Order>::new(&env);
        for i in 0..pending.len() {
            let id = pending.get(i).unwrap();
            if let Some(order) = env.storage().instance().get(&DataKey::Order(id)) {
                orders.push_back(order);
            }
        }
        orders
    }

    /// Update the batch window duration. Admin-only.
    pub fn set_batch_window(env: Env, batch_window_secs: u64) -> Result<(), AuctionError> {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::BatchWindowSecs, &batch_window_secs);
        env.events().publish(
            (Symbol::new(&env, "window_updated"),),
            (batch_window_secs,),
        );
        Ok(())
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use amm::AmmPool;
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        token::{StellarAssetClient, TokenClient as StellarTokenClient},
        Env, String,
    };
    use token::{LpToken, LpTokenClient};

    fn deploy_pool(env: &Env, token_a: &Address, token_b: &Address) -> Address {
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        LpTokenClient::new(env, &lp_addr).initialize(
            &amm_addr,
            &String::from_str(env, "LP"),
            &String::from_str(env, "LP"),
            &7u32,
        );
        AmmPoolClient::new(env, &amm_addr)
            .initialize(
                &amm_addr,
                token_a,
                token_b,
                &lp_addr,
                &30_i128,
                &amm_addr,
                &0_i128,
            )
            .unwrap();
        amm_addr
    }

    fn setup(env: &Env) -> (Address, Address, Address, Address) {
        let admin = Address::generate(env);
        let ta = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tb = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let pool = deploy_pool(env, &ta, &tb);

        let lp = Address::generate(env);
        StellarAssetClient::new(env, &ta).mint(&lp, &2_000_000_i128);
        StellarAssetClient::new(env, &tb).mint(&lp, &2_000_000_i128);
        AmmPoolClient::new(env, &pool).add_liquidity(
            &lp,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        (ta, tb, pool, admin)
    }

    #[test]
    fn test_submit_and_settle() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        BatchAuctionClient::new(&env, &auction_addr)
            .initialize(&admin, &30_u64)
            .unwrap();

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);

        BatchAuctionClient::new(&env, &auction_addr)
            .submit_order(
                &trader, &pool, &ta, &tb,
                &10_000_i128, &0_i128, &u64::MAX,
            )
            .unwrap();

        // Advance past the batch window.
        env.ledger().set_timestamp(1031);

        let results = BatchAuctionClient::new(&env, &auction_addr)
            .settle_batch()
            .unwrap();

        assert_eq!(results.len(), 1);
        assert!(results.get(0).unwrap() > 0);

        // Trader received token_b.
        let tb_balance = StellarTokenClient::new(&env, &tb).balance(&trader);
        assert!(tb_balance > 0);
    }

    #[test]
    fn test_cancel_order_refunds_tokens() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        BatchAuctionClient::new(&env, &auction_addr)
            .initialize(&admin, &30_u64)
            .unwrap();

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);

        let order_id = BatchAuctionClient::new(&env, &auction_addr)
            .submit_order(
                &trader, &pool, &ta, &tb,
                &10_000_i128, &0_i128, &u64::MAX,
            )
            .unwrap();

        // Tokens were escrowed — trader's balance decreased.
        let balance_after_submit = StellarTokenClient::new(&env, &ta).balance(&trader);
        assert_eq!(balance_after_submit, 90_000_i128);

        BatchAuctionClient::new(&env, &auction_addr)
            .cancel_order(&trader, &order_id)
            .unwrap();

        // Tokens returned after cancel.
        let balance_after_cancel = StellarTokenClient::new(&env, &ta).balance(&trader);
        assert_eq!(balance_after_cancel, 100_000_i128);

        let orders = BatchAuctionClient::new(&env, &auction_addr).get_pending_orders();
        assert_eq!(orders.len(), 0);
    }

    #[test]
    fn test_settle_before_window_reverts() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        BatchAuctionClient::new(&env, &auction_addr)
            .initialize(&admin, &30_u64)
            .unwrap();

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);
        BatchAuctionClient::new(&env, &auction_addr)
            .submit_order(&trader, &pool, &ta, &tb, &10_000_i128, &0_i128, &u64::MAX)
            .unwrap();

        // Window has not elapsed — should return BatchWindowOpen error.
        let result = BatchAuctionClient::new(&env, &auction_addr)
            .try_settle_batch();
        assert!(result.is_err());
    }

    #[test]
    fn test_multiple_traders_in_same_batch() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        BatchAuctionClient::new(&env, &auction_addr)
            .initialize(&admin, &60_u64)
            .unwrap();

        let trader1 = Address::generate(&env);
        let trader2 = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader1, &50_000_i128);
        StellarAssetClient::new(&env, &ta).mint(&trader2, &50_000_i128);

        BatchAuctionClient::new(&env, &auction_addr)
            .submit_order(&trader1, &pool, &ta, &tb, &5_000_i128, &0_i128, &u64::MAX)
            .unwrap();
        BatchAuctionClient::new(&env, &auction_addr)
            .submit_order(&trader2, &pool, &ta, &tb, &5_000_i128, &0_i128, &u64::MAX)
            .unwrap();

        env.ledger().set_timestamp(1061);

        let results = BatchAuctionClient::new(&env, &auction_addr)
            .settle_batch()
            .unwrap();

        assert_eq!(results.len(), 2);
        assert!(results.get(0).unwrap() > 0);
        assert!(results.get(1).unwrap() > 0);

        // Both traders received token_b.
        assert!(StellarTokenClient::new(&env, &tb).balance(&trader1) > 0);
        assert!(StellarTokenClient::new(&env, &tb).balance(&trader2) > 0);
    }
}
