use crate::error::{Result, SimulationError};
use crate::pool::PoolState;
use crate::replay::{TradeAction, TradeRecord};
use serde::Serialize;
use std::fs;
use std::path::Path;

pub fn load_pool_state(path: impl AsRef<Path>) -> Result<PoolState> {
    let path = path.as_ref();
    let contents = fs::read_to_string(path).map_err(|source| SimulationError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let pool: PoolState = serde_json::from_str(&contents).map_err(|source| SimulationError::Json {
        path: path.display().to_string(),
        source,
    })?;
    pool.validate()?;
    Ok(pool)
}

pub fn load_trade_records(path: impl AsRef<Path>) -> Result<Vec<TradeRecord>> {
    let path = path.as_ref();
    match path.extension().and_then(|ext| ext.to_str()).unwrap_or_default() {
        "csv" => load_trade_records_csv(path),
        _ => load_trade_records_json(path),
    }
}

pub fn save_json_pretty<T: Serialize>(path: impl AsRef<Path>, value: &T) -> Result<()> {
    let path = path.as_ref();
    let json = serde_json::to_string_pretty(value).map_err(|source| SimulationError::Json {
        path: path.display().to_string(),
        source,
    })?;
    fs::write(path, json).map_err(|source| SimulationError::Io {
        path: path.display().to_string(),
        source,
    })
}

fn load_trade_records_json(path: &Path) -> Result<Vec<TradeRecord>> {
    let contents = fs::read_to_string(path).map_err(|source| SimulationError::Io {
        path: path.display().to_string(),
        source,
    })?;

    if let Ok(records) = serde_json::from_str::<Vec<TradeRecord>>(&contents) {
        return Ok(records);
    }

    #[derive(serde::Deserialize)]
    struct Wrapper {
        trades: Vec<TradeRecord>,
    }

    serde_json::from_str::<Wrapper>(&contents)
        .map(|wrapper| wrapper.trades)
        .map_err(|source| SimulationError::Json {
            path: path.display().to_string(),
            source,
        })
}

fn load_trade_records_csv(path: &Path) -> Result<Vec<TradeRecord>> {
    #[derive(serde::Deserialize)]
    struct Row {
        timestamp: u64,
        kind: String,
        label: Option<String>,
        token_in: Option<String>,
        token_out: Option<String>,
        amount_in: Option<i128>,
        amount_out: Option<i128>,
        amount_a: Option<i128>,
        amount_b: Option<i128>,
        shares: Option<i128>,
        min_out: Option<i128>,
        max_in: Option<i128>,
        min_shares: Option<i128>,
        min_a: Option<i128>,
        min_b: Option<i128>,
    }

    let mut reader = csv::Reader::from_path(path).map_err(|source| SimulationError::Csv {
        path: path.display().to_string(),
        source,
    })?;
    let mut records = Vec::new();

    for row in reader.deserialize::<Row>() {
        let row = row.map_err(|source| SimulationError::Csv {
            path: path.display().to_string(),
            source,
        })?;
        let action = match row.kind.as_str() {
            "swap_exact_in" => TradeAction::SwapExactIn {
                token_in: row.token_in.unwrap_or_default(),
                amount_in: row.amount_in.unwrap_or_default(),
                min_out: row.min_out.unwrap_or_default(),
            },
            "swap_exact_out" => TradeAction::SwapExactOut {
                token_out: row.token_out.unwrap_or_default(),
                amount_out: row.amount_out.unwrap_or_default(),
                max_in: row.max_in,
            },
            "add_liquidity" => TradeAction::AddLiquidity {
                amount_a: row.amount_a.unwrap_or_default(),
                amount_b: row.amount_b.unwrap_or_default(),
                min_shares: row.min_shares.unwrap_or_default(),
            },
            "remove_liquidity" => TradeAction::RemoveLiquidity {
                shares: row.shares.unwrap_or_default(),
                min_a: row.min_a.unwrap_or_default(),
                min_b: row.min_b.unwrap_or_default(),
            },
            other => {
                return Err(SimulationError::InvalidInput(format!(
                    "unknown trade kind `{other}`"
                )))
            }
        };

        records.push(TradeRecord {
            timestamp: row.timestamp,
            label: row.label,
            action,
        });
    }

    Ok(records)
}
