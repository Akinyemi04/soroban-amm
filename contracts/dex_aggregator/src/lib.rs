#![no_std]

//! DEX aggregator — routes trades across multiple AMM and CL pools for best execution.

use soroban_sdk::{contract, contractclient, contractimpl, contracttype, vec, Address, Env, Vec};

use amm::{AmmPoolClient, PoolInfo};
use factory::FactoryClient;

#[contractclient(name = "ClPoolClient")]
pub trait ClPoolInterface {
    fn estimate_price_impact(
        env: Env,
        zero_for_one: bool,
        amount_in: i128,
        sqrt_price_limit_x96: u128,
    ) -> Result<PriceImpactEstimate, ClError>;
    fn swap(
        env: Env,
        sender: Address,
        zero_for_one: bool,
        amount_in: i128,
        sqrt_price_limit_x96: u128,
        min_amount_out: i128,
        deadline: u64,
    ) -> Result<i128, ClError>;
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

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ClError {
    ZeroAmounts = 1,
    DeadlineExpired = 2,
    Paused = 3,
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

    pub fn find_best_route(
        env: Env,
        token_in: Address,
        token_out: Address,
        amount_in: i128,
    ) -> RouteQuote {
        assert!(token_in != token_out, "same token");
        assert!(amount_in > 0, "amount must be positive");
        Self::search_best(&env, &token_in, &token_out, amount_in)
    }

    pub fn swap_best(
        env: Env,
        trader: Address,
        token_in: Address,
        token_out: Address,
        amount_in: i128,
        min_out: i128,
    ) -> i128 {
        trader.require_auth();
        let quote = Self::find_best_route(env.clone(), token_in, token_out, amount_in);
        assert!(quote.amount_out >= min_out, "slippage exceeded");
        assert!(!quote.hops.is_empty(), "no route found");

        let mut current = amount_in;
        let deadline = env.ledger().timestamp() + 3600;
        let last = quote.hops.len() - 1;
        for i in 0..quote.hops.len() {
            let hop = quote.hops.get(i).unwrap();
            let hop_min = if i == last { min_out } else { 0 };
            current = match hop.pool_kind {
                PoolKind::Amm => AmmPoolClient::new(&env, &hop.pool).swap(
                    &trader,
                    &hop.token_in,
                    &current,
                    &hop_min,
                ),
                PoolKind::Cl => ClPoolClient::new(&env, &hop.pool)
                    .swap(
                        &trader,
                        &hop.zero_for_one,
                        &current,
                        &0u128,
                        &hop_min,
                        &deadline,
                    )
                    .unwrap(),
            };
        }
        current
    }

    pub fn is_price_within_tolerance(
        env: Env,
        token_in: Address,
        token_out: Address,
        amount_in: i128,
        quoted_out: i128,
    ) -> bool {
        let best = Self::find_best_route(env, token_in, token_out, amount_in);
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

    fn search_best(env: &Env, token_in: &Address, token_out: &Address, amount_in: i128) -> RouteQuote {
        let factory: Address = env.storage().instance().get(&DataKey::Factory).unwrap();
        let factory_client = FactoryClient::new(env, &factory);
        let intermediates = Self::discover_tokens(env, &factory_client);

        let mut best_out: i128 = 0;
        let mut best_hops: Vec<RouteHop> = Vec::new(env);

        if let Some(h) = Self::quote_hop(env, &factory_client, token_in, token_out, amount_in) {
            best_out = h.amount_out;
            best_hops = h.hops;
        }

        for i in 0..intermediates.len() {
            let mid = intermediates.get(i).unwrap();
            if mid == *token_out {
                continue;
            }
            if let Some(h1) = Self::quote_hop(env, &factory_client, token_in, &mid, amount_in) {
                if let Some(h2) =
                    Self::quote_hop(env, &factory_client, &mid, token_out, h1.amount_out)
                {
                    if h2.amount_out > best_out {
                        best_out = h2.amount_out;
                        best_hops = Self::concat_hops(env, &h1.hops, &h2.hops);
                    }
                }
            }
        }

        for i in 0..intermediates.len() {
            let t1 = intermediates.get(i).unwrap();
            if t1 == *token_in || t1 == *token_out {
                continue;
            }
            let Some(h1) = Self::quote_hop(env, &factory_client, token_in, &t1, amount_in) else {
                continue;
            };
            for j in 0..intermediates.len() {
                let t2 = intermediates.get(j).unwrap();
                if t2 == *token_out || t2 == t1 {
                    continue;
                }
                let Some(h2) = Self::quote_hop(env, &factory_client, &t1, &t2, h1.amount_out) else {
                    continue;
                };
                if let Some(h3) =
                    Self::quote_hop(env, &factory_client, &t2, token_out, h2.amount_out)
                {
                    if h3.amount_out > best_out {
                        best_out = h3.amount_out;
                        let merged = Self::concat_hops(env, &h1.hops, &h2.hops);
                        best_hops = Self::concat_hops(env, &merged, &h3.hops);
                    }
                }
            }
        }

        RouteQuote {
            amount_out: best_out,
            hops: best_hops,
        }
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
            if let Ok(est) = client.estimate_price_impact(&direction, &amount_in, &0u128) {
                if est.amount_out > best {
                    best = est.amount_out;
                    zfo = direction;
                }
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

    fn concat_hops(env: &Env, a: &Vec<RouteHop>, b: &Vec<RouteHop>) -> Vec<RouteHop> {
        let mut out: Vec<RouteHop> = Vec::new(env);
        for i in 0..a.len() {
            out.push_back(a.get(i).unwrap());
        }
        for i in 0..b.len() {
            out.push_back(b.get(i).unwrap());
        }
        out
    }
}

