use crate::error::Result;
use crate::pool::PoolState;
use crate::replay::{ReplayReport, TradeAction, TradeOutcome, TradeRecord};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SimulationStep {
    pub timestamp: u64,
    pub action: TradeAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<TradeOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct AmmSimulator {
    pub pool: PoolState,
    pub steps: Vec<SimulationStep>,
    pub total_amount_in: i128,
    pub total_amount_out: i128,
    pub total_fees: i128,
}

impl AmmSimulator {
    pub fn new(pool: PoolState) -> Self {
        Self {
            pool,
            steps: Vec::new(),
            total_amount_in: 0,
            total_amount_out: 0,
            total_fees: 0,
        }
    }

    pub fn replay(&mut self, trades: &[TradeRecord]) -> Result<ReplayReport> {
        for trade in trades {
            self.apply_trade(trade.clone());
        }
        Ok(ReplayReport::from_simulator(self))
    }

    pub fn apply_trade(&mut self, trade: TradeRecord) {
        let mut error = None;
        let mut outcome = None;

        match self.pool.advance_to(trade.timestamp) {
            Ok(()) => {
                let before = self.pool.clone();
                match self.apply_action(trade.clone()) {
                    Ok(action_outcome) => {
                        outcome = Some(TradeOutcome {
                            record: trade.clone(),
                            before,
                            after: self.pool.clone(),
                            swap: action_outcome.swap,
                            exact_out: action_outcome.exact_out,
                            liquidity: action_outcome.liquidity,
                        });
                    }
                    Err(err) => {
                        error = Some(err.to_string());
                    }
                }
            }
            Err(err) => {
                error = Some(err.to_string());
            }
        }

        self.steps.push(SimulationStep {
            timestamp: trade.timestamp,
            action: trade.action,
            outcome,
            error,
        });
    }

    fn apply_action(&mut self, trade: TradeRecord) -> Result<ActionOutcome> {
        match trade.action {
            TradeAction::SwapExactIn {
                token_in,
                amount_in,
                min_out,
            } => {
                let quote = self.pool.execute_swap_exact_in(&token_in, amount_in, min_out)?;
                self.total_amount_in = self.total_amount_in + amount_in;
                self.total_amount_out = self.total_amount_out + quote.amount_out;
                self.total_fees = self.total_fees + quote.fee_amount;
                Ok(ActionOutcome {
                    swap: Some(quote),
                    exact_out: None,
                    liquidity: None,
                })
            }
            TradeAction::SwapExactOut {
                token_out,
                amount_out,
                max_in,
            } => {
                let quote = self
                    .pool
                    .execute_swap_exact_out(&token_out, amount_out, max_in.unwrap_or(i128::MAX))?;
                self.total_amount_in = self.total_amount_in + quote.amount_in;
                self.total_amount_out = self.total_amount_out + amount_out;
                self.total_fees = self.total_fees + quote.fee_amount;
                Ok(ActionOutcome {
                    swap: None,
                    exact_out: Some(quote),
                    liquidity: None,
                })
            }
            TradeAction::AddLiquidity {
                amount_a,
                amount_b,
                min_shares,
            } => {
                let quote = self.pool.execute_add_liquidity(amount_a, amount_b, min_shares)?;
                Ok(ActionOutcome {
                    swap: None,
                    exact_out: None,
                    liquidity: Some(quote),
                })
            }
            TradeAction::RemoveLiquidity {
                shares,
                min_a,
                min_b,
            } => {
                let quote = self.pool.execute_remove_liquidity(shares, min_a, min_b)?;
                Ok(ActionOutcome {
                    swap: None,
                    exact_out: None,
                    liquidity: Some(quote),
                })
            }
        }
    }
}

#[derive(Clone, Debug)]
struct ActionOutcome {
    swap: Option<crate::pool::SwapQuote>,
    exact_out: Option<crate::pool::SwapResult>,
    liquidity: Option<crate::pool::LiquidityQuote>,
}
