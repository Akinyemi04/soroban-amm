use crate::pool::{LiquidityQuote, PoolState, SwapQuote, SwapResult};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TradeAction {
    SwapExactIn {
        token_in: String,
        amount_in: i128,
        #[serde(default)]
        min_out: i128,
    },
    SwapExactOut {
        token_out: String,
        amount_out: i128,
        #[serde(default)]
        max_in: Option<i128>,
    },
    AddLiquidity {
        amount_a: i128,
        amount_b: i128,
        #[serde(default)]
        min_shares: i128,
    },
    RemoveLiquidity {
        shares: i128,
        #[serde(default)]
        min_a: i128,
        #[serde(default)]
        min_b: i128,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TradeRecord {
    pub timestamp: u64,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(flatten)]
    pub action: TradeAction,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TradeOutcome {
    pub record: TradeRecord,
    pub before: PoolState,
    pub after: PoolState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swap: Option<SwapQuote>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exact_out: Option<SwapResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub liquidity: Option<LiquidityQuote>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ReplaySummary {
    pub trades: usize,
    pub successful_trades: usize,
    pub failed_trades: usize,
    pub total_amount_in: i128,
    pub total_amount_out: i128,
    pub total_fees: i128,
    pub final_pool: PoolState,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ReplayReport {
    pub summary: ReplaySummary,
    pub steps: Vec<crate::engine::SimulationStep>,
}

impl ReplayReport {
    pub fn from_simulator(simulator: &crate::engine::AmmSimulator) -> Self {
        Self {
            summary: ReplaySummary {
                trades: simulator.steps.len(),
                successful_trades: simulator
                    .steps
                    .iter()
                    .filter(|step| step.error.is_none())
                    .count(),
                failed_trades: simulator
                    .steps
                    .iter()
                    .filter(|step| step.error.is_some())
                    .count(),
                total_amount_in: simulator.total_amount_in,
                total_amount_out: simulator.total_amount_out,
                total_fees: simulator.total_fees,
                final_pool: simulator.pool.clone(),
            },
            steps: simulator.steps.clone(),
        }
    }
}
