use crate::engine::AmmSimulator;
use crate::error::Result;
use crate::pool::PoolState;
use crate::replay::TradeRecord;
use rand::{rngs::SmallRng, seq::SliceRandom, Rng, SeedableRng};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MonteCarloConfig {
    pub iterations: usize,
    #[serde(default)]
    pub amount_shock_bps: u32,
    #[serde(default)]
    pub shuffle_trades: bool,
    #[serde(default)]
    pub seed: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MonteCarloReport {
    pub config: MonteCarloConfig,
    pub base_pool: PoolState,
    pub trades: usize,
    pub successful_paths: usize,
    pub failed_paths: usize,
    pub mean_final_reserve_a: f64,
    pub mean_final_reserve_b: f64,
    pub mean_final_price: f64,
    pub median_final_reserve_a: i128,
    pub median_final_reserve_b: i128,
    pub p95_final_reserve_a: i128,
    pub p95_final_reserve_b: i128,
    pub min_final_reserve_a: i128,
    pub max_final_reserve_a: i128,
    pub min_final_reserve_b: i128,
    pub max_final_reserve_b: i128,
}

impl MonteCarloReport {
    pub fn run(base_pool: &PoolState, trades: &[TradeRecord], config: MonteCarloConfig) -> Result<Self> {
        let mut rng = SmallRng::seed_from_u64(config.seed);
        let mut reserve_a_samples = Vec::with_capacity(config.iterations);
        let mut reserve_b_samples = Vec::with_capacity(config.iterations);
        let mut price_samples = Vec::with_capacity(config.iterations);
        let mut successful_paths = 0usize;
        let mut failed_paths = 0usize;

        for _ in 0..config.iterations {
            let mut simulator = AmmSimulator::new(base_pool.clone());
            let mut scenario = trades.to_vec();
            if config.shuffle_trades {
                scenario.shuffle(&mut rng);
                let start_ts = scenario.first().map(|trade| trade.timestamp).unwrap_or(0);
                for (idx, trade) in scenario.iter_mut().enumerate() {
                    trade.timestamp = start_ts + idx as u64;
                }
            }
            let scenario = perturb_trades(&scenario, config.amount_shock_bps, &mut rng);
            simulator.replay(&scenario)?;
            if simulator.steps.iter().any(|step| step.error.is_some()) {
                failed_paths += 1;
            } else {
                successful_paths += 1;
                reserve_a_samples.push(simulator.pool.reserve_a);
                reserve_b_samples.push(simulator.pool.reserve_b);
                price_samples.push(simulator.pool.spot_price_a() as f64);
            }
        }

        if reserve_a_samples.is_empty() {
            return Ok(Self {
                config,
                base_pool: base_pool.clone(),
                trades: trades.len(),
                successful_paths,
                failed_paths,
                mean_final_reserve_a: 0.0,
                mean_final_reserve_b: 0.0,
                mean_final_price: 0.0,
                median_final_reserve_a: 0,
                median_final_reserve_b: 0,
                p95_final_reserve_a: 0,
                p95_final_reserve_b: 0,
                min_final_reserve_a: 0,
                max_final_reserve_a: 0,
                min_final_reserve_b: 0,
                max_final_reserve_b: 0,
            });
        }

        reserve_a_samples.sort_unstable();
        reserve_b_samples.sort_unstable();

        let mean_final_reserve_a = reserve_a_samples.iter().map(|v| *v as f64).sum::<f64>()
            / reserve_a_samples.len() as f64;
        let mean_final_reserve_b = reserve_b_samples.iter().map(|v| *v as f64).sum::<f64>()
            / reserve_b_samples.len() as f64;
        let mean_final_price = price_samples.iter().sum::<f64>() / price_samples.len() as f64;

        let median_index = reserve_a_samples.len() / 2;
        let p95_index = ((reserve_a_samples.len() as f64) * 0.95).floor() as usize;
        let p95_index = p95_index.min(reserve_a_samples.len() - 1);

        Ok(Self {
            config,
            base_pool: base_pool.clone(),
            trades: trades.len(),
            successful_paths,
            failed_paths,
            mean_final_reserve_a,
            mean_final_reserve_b,
            mean_final_price,
            median_final_reserve_a: reserve_a_samples[median_index],
            median_final_reserve_b: reserve_b_samples[median_index],
            p95_final_reserve_a: reserve_a_samples[p95_index],
            p95_final_reserve_b: reserve_b_samples[p95_index],
            min_final_reserve_a: *reserve_a_samples.first().unwrap(),
            max_final_reserve_a: *reserve_a_samples.last().unwrap(),
            min_final_reserve_b: *reserve_b_samples.first().unwrap(),
            max_final_reserve_b: *reserve_b_samples.last().unwrap(),
        })
    }
}

fn perturb_trades(trades: &[TradeRecord], amount_shock_bps: u32, rng: &mut SmallRng) -> Vec<TradeRecord> {
    if amount_shock_bps == 0 {
        return trades.to_vec();
    }

    trades
        .iter()
        .cloned()
        .map(|mut trade| {
            trade.action = match trade.action {
                crate::replay::TradeAction::SwapExactIn {
                    token_in,
                    amount_in,
                    min_out,
                } => crate::replay::TradeAction::SwapExactIn {
                    token_in,
                    amount_in: shock_amount(amount_in, amount_shock_bps, rng),
                    min_out,
                },
                crate::replay::TradeAction::SwapExactOut {
                    token_out,
                    amount_out,
                    max_in,
                } => crate::replay::TradeAction::SwapExactOut {
                    token_out,
                    amount_out: shock_amount(amount_out, amount_shock_bps, rng),
                    max_in,
                },
                crate::replay::TradeAction::AddLiquidity {
                    amount_a,
                    amount_b,
                    min_shares,
                } => crate::replay::TradeAction::AddLiquidity {
                    amount_a: shock_amount(amount_a, amount_shock_bps, rng),
                    amount_b: shock_amount(amount_b, amount_shock_bps, rng),
                    min_shares,
                },
                crate::replay::TradeAction::RemoveLiquidity {
                    shares,
                    min_a,
                    min_b,
                } => crate::replay::TradeAction::RemoveLiquidity {
                    shares: shock_amount(shares, amount_shock_bps, rng),
                    min_a,
                    min_b,
                },
            };
            trade
        })
        .collect()
}

fn shock_amount(amount: i128, shock_bps: u32, rng: &mut SmallRng) -> i128 {
    let shock = rng.gen_range(-(shock_bps as i128)..=(shock_bps as i128));
    let perturbed = amount + amount * shock / 10_000;
    perturbed.max(1)
}
