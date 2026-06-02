#![no_std]

//! DEX aggregator — routes trades across multiple AMM and CL pools for best execution.

use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype, vec, Address, Env, Vec,
};

use amm::{AmmPoolClient, PoolInfo};
use factory::FactoryClient;

#[contractclient(name = "ClPoolClient")]
pub trait ClPoolInterface {
    fn estimate_price_impact(
        env: Env,
        zero_for_one: bool,
        amount_in: i128,
        sqrt_price_limit_x96: u128,
    ) -> PriceImpactEstimate;
    fn swap(
        env: Env,
        sender: Address,
        zero_for_one: bool,
        amount_in: i128,
        sqrt_price_limit_x96: u128,
        min_amount_out: i128,
        deadline: u64,
    ) -> i128;
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceImpactEstimate {
    pub amount_in: i128,
    pub amount_in_after_fee: i128,
    pub amount_out: i128,
    pub fee_amount: i128,
    pub spot_price_before: i128,
    pub effective_price: i128,
    pub price_impact_bps: i128,
    pub sqrt_price_before: u128,
    pub sqrt_price_after: u128,
    pub tick_before: i32,
    pub tick_after: i32,
    pub active_liquidity_before: i128,
    pub active_liquidity_after: i128,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum AggregatorError {
    NoRouteFound = 1,
    SlippageExceeded = 2,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PoolKind {
    Amm,
    Cl,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteHop {
    pub pool: Address,
    pub pool_kind: PoolKind,
    pub token_in: Address,
    pub token_out: Address,
    pub zero_for_one: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteQuote {
    pub amount_out: i128,
    pub hops: Vec<RouteHop>,
}

#[contracttype]
pub enum DataKey {
    Factory,
    MaxHops,
}

#[contract]
pub struct DexAggregator;

#[contractimpl]
impl DexAggregator {
    pub const DEFAULT_MAX_HOPS: u32 = 4;
    pub const PRICE_TOLERANCE_BPS: i128 = 10;
    pub const BPS: i128 = 10_000;
    pub const CL_FEE_TIERS: [i128; 3] = [30, 100, 500];

    pub fn initialize(env: Env, factory: Address) {
        assert!(
            !env.storage().instance().has(&DataKey::Factory),
            "already initialized"
        );
        env.storage().instance().set(&DataKey::Factory, &factory);
        env.storage()
            .instance()
            .set(&DataKey::MaxHops, &Self::DEFAULT_MAX_HOPS);
    }

    /// Find the best route up to `max_hops` pools deep (#319).
    pub fn find_best_route(
        env: Env,
        token_in: Address,
        token_out: Address,
        amount_in: i128,
        max_hops: u32,
    ) -> Result<RouteQuote, AggregatorError> {
        assert!(token_in != token_out, "same token");
        assert!(amount_in > 0, "amount must be positive");
        let cap = max_hops.min(Self::DEFAULT_MAX_HOPS);
        if cap == 0 {
            return Err(AggregatorError::NoRouteFound);
        }
        Self::search_best_bfs(&env, &token_in, &token_out, amount_in, cap)
    }

    /// Read-only quote for off-chain simulation (#319).
    pub fn get_quote(
        env: Env,
        token_in: Address,
        token_out: Address,
        amount_in: i128,
        max_hops: u32,
    ) -> Result<RouteQuote, AggregatorError> {
        Self::find_best_route(env, token_in, token_out, amount_in, max_hops)
    }

    /// Execute a pre-computed multi-hop route atomically (#319).
    pub fn execute_route(
        env: Env,
        route: RouteQuote,
        trader: Address,
        amount_in: i128,
        min_out: i128,
        deadline: u64,
    ) -> Result<i128, AggregatorError> {
        trader.require_auth();
        if route.hops.is_empty() || route.amount_out < min_out {
            return Err(AggregatorError::SlippageExceeded);
        }
        if deadline < env.ledger().timestamp() {
            return Err(AggregatorError::SlippageExceeded);
        }
        Self::execute_hops(&env, &route.hops, &trader, amount_in, min_out, deadline)
    }

    pub fn swap_best(
        env: Env,
        trader: Address,
        token_in: Address,
        token_out: Address,
        amount_in: i128,
        min_out: i128,
    ) -> Result<i128, AggregatorError> {
        let max_hops: u32 = env
            .storage()
            .instance()
            .get(&DataKey::MaxHops)
            .unwrap_or(Self::DEFAULT_MAX_HOPS);
        let quote = Self::find_best_route(
            env.clone(),
            token_in,
            token_out,
            amount_in,
            max_hops,
        )?;
        let deadline = env.ledger().timestamp() + 3600;
        Self::execute_route(env, quote, trader, amount_in, min_out, deadline)
    }

    pub fn is_price_within_tolerance(
        env: Env,
        token_in: Address,
        token_out: Address,
        amount_in: i128,
        quoted_out: i128,
    ) -> bool {
        let max_hops: u32 = env
            .storage()
            .instance()
            .get(&DataKey::MaxHops)
            .unwrap_or(Self::DEFAULT_MAX_HOPS);
        let Ok(best) = Self::find_best_route(env, token_in, token_out, amount_in, max_hops) else {
            return quoted_out == 0;
        };
        if best.amount_out == 0 {
            return quoted_out == 0;
        }
        let diff = if best.amount_out >= quoted_out {
            best.amount_out - quoted_out
        } else {
            quoted_out - best.amount_out
        };
        diff * Self::BPS / best.amount_out <= Self::PRICE_TOLERANCE_BPS
    }

    fn search_best_bfs(
        env: &Env,
        token_in: &Address,
        token_out: &Address,
        amount_in: i128,
        max_hops: u32,
    ) -> Result<RouteQuote, AggregatorError> {
        let factory: Address = env.storage().instance().get(&DataKey::Factory).unwrap();
        let factory_client = FactoryClient::new(env, &factory);
        let tokens = Self::discover_tokens(env, &factory_client);

        let mut best_out: i128 = 0;
        let mut best_hops: Vec<RouteHop> = Vec::new(env);

        let mut frontier_token: Vec<Address> = Vec::new(env);
        let mut frontier_amount: Vec<i128> = Vec::new(env);
        let mut frontier_hops: Vec<Vec<RouteHop>> = Vec::new(env);
        let mut frontier_depth: Vec<u32> = Vec::new(env);

        frontier_token.push_back(token_in.clone());
        frontier_amount.push_back(amount_in);
        frontier_hops.push_back(Vec::new(env));
        frontier_depth.push_back(0);

        let mut idx: u32 = 0;
        while idx < frontier_token.len() {
            let current_token = frontier_token.get(idx).unwrap();
            let current_amount = frontier_amount.get(idx).unwrap();
            let current_hops = frontier_hops.get(idx).unwrap();
            let depth = frontier_depth.get(idx).unwrap();
            idx += 1;

            if depth >= max_hops {
                continue;
            }

            for t in 0..tokens.len() {
                let next_token = tokens.get(t).unwrap();
                if next_token == current_token {
                    continue;
                }

                let Some(step) = Self::quote_hop(
                    env,
                    &factory_client,
                    &current_token,
                    &next_token,
                    current_amount,
                ) else {
                    continue;
                };

                let mut new_hops = Vec::new(env);
                for h in 0..current_hops.len() {
                    new_hops.push_back(current_hops.get(h).unwrap());
                }
                new_hops.push_back(step.hops.get(0).unwrap());

                if next_token == *token_out {
                    if step.amount_out > best_out {
                        best_out = step.amount_out;
                        best_hops = new_hops;
                    }
                } else if depth + 1 < max_hops {
                    frontier_token.push_back(next_token);
                    frontier_amount.push_back(step.amount_out);
                    frontier_hops.push_back(new_hops);
                    frontier_depth.push_back(depth + 1);
                }
            }
        }

        if best_out <= 0 || best_hops.is_empty() {
            return Err(AggregatorError::NoRouteFound);
        }
        Ok(RouteQuote {
            amount_out: best_out,
            hops: best_hops,
        })
    }

    fn execute_hops(
        env: &Env,
        hops: &Vec<RouteHop>,
        trader: &Address,
        amount_in: i128,
        min_out: i128,
        deadline: u64,
    ) -> Result<i128, AggregatorError> {
        let mut current = amount_in;
        let last = hops.len() - 1;
        for i in 0..hops.len() {
            let hop = hops.get(i).unwrap();
            let hop_min = if i == last { min_out } else { 0 };
            current = match hop.pool_kind {
                PoolKind::Amm => AmmPoolClient::new(env, &hop.pool)
                    .swap(trader, &hop.token_in, &current, &hop_min, &deadline),
                PoolKind::Cl => ClPoolClient::new(env, &hop.pool)
                    .swap(
                        trader,
                        &hop.zero_for_one,
                        &current,
                        &0u128,
                        &hop_min,
                        &deadline,
                    ),
            };
        }
        Ok(current)
    }

    fn quote_hop(
        env: &Env,
        factory: &FactoryClient,
        token_in: &Address,
        token_out: &Address,
        amount_in: i128,
    ) -> Option<RouteQuote> {
        if amount_in <= 0 {
            return None;
        }

        let mut best: i128 = 0;
        let mut hop = RouteHop {
            pool: token_in.clone(),
            pool_kind: PoolKind::Amm,
            token_in: token_in.clone(),
            token_out: token_out.clone(),
            zero_for_one: true,
        };

        if let Some(pool) = factory.get_pool(token_in, token_out) {
            let out = AmmPoolClient::new(env, &pool).get_amount_out(token_in, &amount_in);
            if out > best {
                best = out;
                hop = RouteHop {
                    pool,
                    pool_kind: PoolKind::Amm,
                    token_in: token_in.clone(),
                    token_out: token_out.clone(),
                    zero_for_one: true,
                };
            }
        }

        for fee_idx in 0..3 {
            let fee = Self::CL_FEE_TIERS[fee_idx as usize];
            if let Some(cl) = factory.get_cl_pool(token_in, token_out, &fee) {
                if let Some((out, zfo)) =
                    Self::quote_cl(env, &cl, token_in, token_out, amount_in)
                {
                    if out > best {
                        best = out;
                        hop = RouteHop {
                            pool: cl,
                            pool_kind: PoolKind::Cl,
                            token_in: token_in.clone(),
                            token_out: token_out.clone(),
                            zero_for_one: zfo,
                        };
                    }
                }
            }
        }

        if best <= 0 {
            return None;
        }

        Some(RouteQuote {
            amount_out: best,
            hops: vec![env, hop],
        })
    }

    fn quote_cl(
        env: &Env,
        pool: &Address,
        _token_in: &Address,
        _token_out: &Address,
        amount_in: i128,
    ) -> Option<(i128, bool)> {
        let client = ClPoolClient::new(env, pool);
        let mut best: i128 = 0;
        let mut zfo = true;
        for direction in [true, false] {
            let est = client.estimate_price_impact(&direction, &amount_in, &0u128);
            if est.amount_out > best {
                best = est.amount_out;
                zfo = direction;
            }
        }
        if best > 0 {
            Some((best, zfo))
        } else {
            None
        }
    }

    fn discover_tokens(env: &Env, factory: &FactoryClient) -> Vec<Address> {
        let pools = factory.all_pools();
        let mut tokens: Vec<Address> = Vec::new(env);
        for i in 0..pools.len() {
            let pool = pools.get(i).unwrap();
            let info: PoolInfo = AmmPoolClient::new(env, &pool).get_info();
            Self::push_unique(&mut tokens, info.token_a);
            Self::push_unique(&mut tokens, info.token_b);
        }
        tokens
    }

    fn push_unique(vec: &mut Vec<Address>, addr: Address) {
        for i in 0..vec.len() {
            if vec.get(i).unwrap() == addr {
                return;
            }
        }
        vec.push_back(addr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::Address;

    #[test]
    fn test_no_route_when_uninitialized() {
        let env = Env::default();
        let agg_addr = env.register_contract(None, DexAggregator);
        let agg = DexAggregatorClient::new(&env, &agg_addr);
        let a = Address::generate(&env);
        let b = Address::generate(&env);
        let result = agg.try_find_best_route(&a, &b, &100_i128, &3u32);
        assert!(result.is_err());
    }
}
