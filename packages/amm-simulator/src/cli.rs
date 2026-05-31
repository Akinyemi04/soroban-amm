use crate::engine::AmmSimulator;
use crate::error::{Result, SimulationError};
use crate::io::{load_pool_state, load_trade_records};
use crate::monte_carlo::{MonteCarloConfig, MonteCarloReport};
use clap::{Args, Parser, Subcommand};
use std::io::{self, Write};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "amm-sim", about = "Off-chain Soroban AMM simulator")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Quote a swap without mutating pool state.
    Quote(QuoteArgs),
    /// Replay a historical trade log.
    Replay(ReplayArgs),
    /// Run a Monte Carlo stress test against a trade log.
    MonteCarlo(MonteCarloArgs),
}

#[derive(Args, Debug)]
pub struct QuoteArgs {
    /// Pool state JSON file.
    #[arg(long)]
    pub pool: PathBuf,
    /// Token being sold.
    #[arg(long)]
    pub token_in: Option<String>,
    /// Exact input amount.
    #[arg(long)]
    pub amount_in: Option<i128>,
    /// Token being bought.
    #[arg(long)]
    pub token_out: Option<String>,
    /// Exact output amount.
    #[arg(long)]
    pub amount_out: Option<i128>,
    /// Pretty-print JSON output.
    #[arg(long, default_value_t = false)]
    pub pretty: bool,
}

#[derive(Args, Debug)]
pub struct ReplayArgs {
    /// Pool state JSON file.
    #[arg(long)]
    pub pool: PathBuf,
    /// JSON or CSV trade log.
    #[arg(long)]
    pub trades: PathBuf,
    /// Pretty-print JSON output.
    #[arg(long, default_value_t = true)]
    pub pretty: bool,
}

#[derive(Args, Debug)]
pub struct MonteCarloArgs {
    /// Pool state JSON file.
    #[arg(long)]
    pub pool: PathBuf,
    /// JSON or CSV trade log.
    #[arg(long)]
    pub trades: PathBuf,
    /// Number of Monte Carlo runs.
    #[arg(long, default_value_t = 1000)]
    pub iterations: usize,
    /// Amount perturbation in basis points.
    #[arg(long, default_value_t = 0)]
    pub amount_shock_bps: u32,
    /// Shuffle trade order before each run.
    #[arg(long, default_value_t = false)]
    pub shuffle_trades: bool,
    /// RNG seed for reproducible runs.
    #[arg(long, default_value_t = 0)]
    pub seed: u64,
    /// Pretty-print JSON output.
    #[arg(long, default_value_t = true)]
    pub pretty: bool,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Quote(args) => quote(args),
        Commands::Replay(args) => replay(args),
        Commands::MonteCarlo(args) => monte_carlo(args),
    }
}

fn quote(args: QuoteArgs) -> Result<()> {
    let QuoteArgs {
        pool,
        token_in,
        amount_in,
        token_out,
        amount_out,
        pretty,
    } = args;
    let pool = load_pool_state(&pool)?;
    let stdout = io::stdout();
    let mut handle = stdout.lock();

    if let (Some(token_in), Some(amount_in)) = (token_in, amount_in) {
        let quote = pool.quote_swap_exact_in(&token_in, amount_in)?;
        if pretty {
            serde_json::to_writer_pretty(&mut handle, &quote).unwrap();
        } else {
            serde_json::to_writer(&mut handle, &quote).unwrap();
        }
    } else if let (Some(token_out), Some(amount_out)) = (token_out, amount_out) {
        let quote = pool.quote_swap_exact_out(&token_out, amount_out)?;
        if pretty {
            serde_json::to_writer_pretty(&mut handle, &quote).unwrap();
        } else {
            serde_json::to_writer(&mut handle, &quote).unwrap();
        }
    } else {
        return Err(SimulationError::InvalidInput(
            "provide either --token-in + --amount-in or --token-out + --amount-out".into(),
        ));
    }

    writeln!(&mut handle).ok();
    Ok(())
}

fn replay(args: ReplayArgs) -> Result<()> {
    let pool = load_pool_state(&args.pool)?;
    let trades = load_trade_records(&args.trades)?;
    let mut simulator = AmmSimulator::new(pool);
    let report = simulator.replay(&trades)?;
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    if args.pretty {
        serde_json::to_writer_pretty(&mut handle, &report).unwrap();
    } else {
        serde_json::to_writer(&mut handle, &report).unwrap();
    }
    writeln!(&mut handle).ok();
    Ok(())
}

fn monte_carlo(args: MonteCarloArgs) -> Result<()> {
    let pool = load_pool_state(&args.pool)?;
    let trades = load_trade_records(&args.trades)?;
    let report = MonteCarloReport::run(
        &pool,
        &trades,
        MonteCarloConfig {
            iterations: args.iterations,
            amount_shock_bps: args.amount_shock_bps,
            shuffle_trades: args.shuffle_trades,
            seed: args.seed,
        },
    )?;
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    if args.pretty {
        serde_json::to_writer_pretty(&mut handle, &report).unwrap();
    } else {
        serde_json::to_writer(&mut handle, &report).unwrap();
    }
    writeln!(&mut handle).ok();
    Ok(())
}
