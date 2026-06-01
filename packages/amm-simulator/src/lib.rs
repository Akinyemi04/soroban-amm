//! Off-chain Soroban AMM simulation engine.
//!
//! The simulator mirrors the on-chain constant-product math used by
//! `contracts/amm` so strategy testing, historical replay, and Monte Carlo
//! experiments can use the same pricing model without paying gas.

mod cli;
mod engine;
mod error;
mod io;
mod monte_carlo;
mod pool;
mod replay;

pub use cli::{run, Cli};
pub use engine::{AmmSimulator, SimulationStep};
pub use error::SimulationError;
pub use monte_carlo::{MonteCarloConfig, MonteCarloReport};
pub use pool::{LiquidityQuote, PoolState, SwapQuote, SwapResult};
pub use replay::{ReplayReport, ReplaySummary, TradeAction, TradeOutcome, TradeRecord};

pub use io::{load_pool_state, load_trade_records, save_json_pretty};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_quote_matches_on_chain_formula() {
        let pool = PoolState::new("A", "B", 30).unwrap();
        let mut pool = PoolState {
            reserve_a: 1_000_000,
            reserve_b: 1_000_000,
            total_shares: 1_000_000,
            ..pool
        };

        let quote = pool.quote_swap_exact_in("A", 100_000).unwrap();
        assert_eq!(quote.amount_out, 90_661);
        assert_eq!(quote.fee_amount, 300);
        assert!(quote.price_impact_bps > 0);
    }

    #[test]
    fn replay_tracks_success_and_failure() {
        let pool = PoolState {
            token_a: "A".into(),
            token_b: "B".into(),
            reserve_a: 1_000_000,
            reserve_b: 1_000_000,
            total_shares: 1_000_000,
            fee_bps: 30,
            protocol_fee_bps: 0,
            accrued_fee_a: 0,
            accrued_fee_b: 0,
            price_cumulative_a: 0,
            price_cumulative_b: 0,
            last_timestamp: 0,
            paused: false,
        };
        let trades = vec![
            TradeRecord {
                timestamp: 1,
                label: None,
                action: TradeAction::SwapExactIn {
                    token_in: "A".into(),
                    amount_in: 100_000,
                    min_out: 0,
                },
            },
            TradeRecord {
                timestamp: 2,
                label: None,
                action: TradeAction::SwapExactOut {
                    token_out: "B".into(),
                    amount_out: 50_000,
                    max_in: Some(1),
                },
            },
        ];

        let mut simulator = AmmSimulator::new(pool);
        let report = simulator.replay(&trades).unwrap();
        assert_eq!(report.summary.trades, 2);
        assert_eq!(report.summary.successful_trades, 1);
        assert_eq!(report.summary.failed_trades, 1);
        assert_eq!(report.steps.len(), 2);
    }
}
