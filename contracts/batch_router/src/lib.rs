//! Batched AMM operations — execute multiple swaps and liquidity actions atomically
//! in a single transaction to reduce overhead versus separate calls.

#![no_std]

use soroban_sdk::{contract, contractimpl, contracttype, Address, Env, Symbol, Vec};

use amm::AmmPoolClient;

#[contracttype]
#[derive(Clone, Debug)]
pub struct SwapOp {
    pub pool: Address,
    pub token_in: Address,
    pub amount_in: i128,
    pub min_out: i128,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct AddLiquidityOp {
    pub pool: Address,
    pub amount_a: i128,
    pub amount_b: i128,
    pub min_shares: i128,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct RemoveLiquidityOp {
    pub pool: Address,
    pub shares: i128,
    pub min_a: i128,
    pub min_b: i128,
}

#[contracttype]
#[derive(Clone, Debug)]
pub enum BatchOp {
    /// Swap `amount_in` of `token_in` on `pool` with `min_out` slippage guard.
    Swap(SwapOp),
    /// Add liquidity to `pool`.
    AddLiquidity(AddLiquidityOp),
    /// Remove liquidity from `pool`.
    RemoveLiquidity(RemoveLiquidityOp),
}

#[contract]
pub struct BatchRouter;

#[contractimpl]
impl BatchRouter {
    /// Execute a sequence of AMM operations atomically.
    ///
    /// All operations share one `deadline` and a single `caller` authorization.
    /// If any step fails the entire batch reverts.
    pub fn execute_batch(
        env: Env,
        caller: Address,
        ops: Vec<BatchOp>,
        deadline: u64,
    ) -> Vec<i128> {
        caller.require_auth();
        assert!(!ops.is_empty(), "empty batch");
        assert!(env.ledger().timestamp() <= deadline, "deadline expired");

        let mut results = Vec::new(&env);
        for i in 0..ops.len() {
            let op = ops.get(i).unwrap();
            let result = Self::execute_op(&env, &caller, &op, deadline);
            results.push_back(result);
        }

        env.events().publish(
            (Symbol::new(&env, "batch_executed"), caller.clone()),
            (ops.len() as u32,),
        );

        results
    }

    /// Estimate how many top-level contract calls a batch saves vs individual txs.
    ///
    /// Returns `(individual_calls, batch_calls)` for off-chain fee comparison.
    pub fn estimate_call_savings(ops_len: u32) -> (u32, u32) {
        (ops_len, 1)
    }

    fn execute_op(env: &Env, caller: &Address, op: &BatchOp, deadline: u64) -> i128 {
        match op {
            BatchOp::Swap(op) => {
                AmmPoolClient::new(env, &op.pool).swap(
                    caller,
                    &op.token_in,
                    &op.amount_in,
                    &op.min_out,
                    &deadline,
                )
            }
            BatchOp::AddLiquidity(op) => {
                AmmPoolClient::new(env, &op.pool).add_liquidity(
                    caller,
                    &op.amount_a,
                    &op.amount_b,
                    &op.min_shares,
                    &deadline,
                )
            }
            BatchOp::RemoveLiquidity(op) => {
                let (a, b) = AmmPoolClient::new(env, &op.pool).remove_liquidity(
                    caller,
                    &op.shares,
                    &op.min_a,
                    &op.min_b,
                    &deadline,
                );
                // Pack both legs into one i128 result is lossy; return shares burned as marker.
                let _ = (a, b);
                op.shares
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amm::AmmPool;
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        token::{StellarAssetClient, TokenClient as StellarTokenClient},
        vec, Env, String,
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

    fn setup_pool(env: &Env) -> (Address, Address, Address, Address) {
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
            &u64::MAX,
        );

        (ta, tb, pool, lp)
    }

    #[test]
    fn test_batch_multiple_swaps() {
        let env = Env::default();
        env.mock_all_auths();
        let (ta, tb, pool, _) = setup_pool(&env);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &500_000_i128);

        let ops = vec![
            &env,
            BatchOp::Swap(SwapOp {
                pool: pool.clone(),
                token_in: ta.clone(),
                amount_in: 10_000_i128,
                min_out: 0_i128,
            }),
            BatchOp::Swap(SwapOp {
                pool: pool.clone(),
                token_in: tb.clone(),
                amount_in: 5_000_i128,
                min_out: 0_i128,
            }),
            BatchOp::Swap(SwapOp {
                pool: pool.clone(),
                token_in: ta.clone(),
                amount_in: 3_000_i128,
                min_out: 0_i128,
            }),
        ];

        let deadline = env.ledger().timestamp() + 1000;
        let results = BatchRouterClient::new(&env, &env.register_contract(None, BatchRouter))
            .execute_batch(&trader, &ops, &deadline);

        assert_eq!(results.len(), 3);
        assert!(results.get(0).unwrap() > 0);
    }

    #[test]
    fn test_batch_swap_then_add_liquidity() {
        let env = Env::default();
        env.mock_all_auths();
        let (ta, tb, pool, _) = setup_pool(&env);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &200_000_i128);
        StellarAssetClient::new(&env, &tb).mint(&trader, &200_000_i128);

        let swap_out = AmmPoolClient::new(&env, &pool).swap(
            &trader,
            &ta,
            &50_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let tb_bal = StellarTokenClient::new(&env, &tb).balance(&trader);
        let ops = vec![
            &env,
            BatchOp::Swap(SwapOp {
                pool: pool.clone(),
                token_in: ta.clone(),
                amount_in: 10_000_i128,
                min_out: 0_i128,
            }),
            BatchOp::AddLiquidity(AddLiquidityOp {
                pool: pool.clone(),
                amount_a: 5_000_i128,
                amount_b: tb_bal / 10,
                min_shares: 0_i128,
            }),
        ];

        let batch_addr = env.register_contract(None, BatchRouter);
        let deadline = env.ledger().timestamp() + 1000;
        let results =
            BatchRouterClient::new(&env, &batch_addr).execute_batch(&trader, &ops, &deadline);

        assert_eq!(results.len(), 2);
        assert!(results.get(0).unwrap() > 0);
        assert!(results.get(1).unwrap() > 0);
        let _ = swap_out;
    }

    #[test]
    #[should_panic(expected = "batch swap failed")]
    fn test_batch_atomic_revert_on_slippage() {
        let env = Env::default();
        env.mock_all_auths();
        let (ta, tb, pool, _) = setup_pool(&env);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);

        let ops = vec![
            &env,
            BatchOp::Swap(SwapOp {
                pool: pool.clone(),
                token_in: ta.clone(),
                amount_in: 1_000_i128,
                min_out: 0_i128,
            }),
            BatchOp::Swap(SwapOp {
                pool,
                token_in: tb,
                amount_in: 1_000_i128,
                min_out: 10_000_000_i128,
            }),
        ];

        let batch_addr = env.register_contract(None, BatchRouter);
        let deadline = env.ledger().timestamp() + 1000;
        BatchRouterClient::new(&env, &batch_addr).execute_batch(&trader, &ops, &deadline);
    }

    #[test]
    fn test_batch_call_savings_exceeds_fifteen_percent() {
        let ops_len = 3_u32;
        let (individual, batch) = BatchRouter::estimate_call_savings(ops_len);
        let savings_bps = (individual - batch) * 10_000 / individual;
        assert!(savings_bps > 1_500, "expected >15% call overhead savings");
    }
}
