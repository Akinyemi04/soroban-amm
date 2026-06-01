use thiserror::Error;

#[derive(Debug, Error)]
pub enum SimulationError {
    #[error("amount must be positive")]
    ZeroAmount,
    #[error("token `{token}` is not part of the pool")]
    InvalidToken { token: String },
    #[error("pool has no liquidity")]
    EmptyPool,
    #[error("slippage guard failed")]
    SlippageExceeded,
    #[error("insufficient LP shares")]
    InsufficientShares,
    #[error("deadline exceeded at {deadline}")]
    DeadlineExceeded { deadline: u64 },
    #[error("pool is paused")]
    Paused,
    #[error("{0}")]
    InvalidInput(String),
    #[error("invalid fee bps {fee_bps}")]
    InvalidFeeBps { fee_bps: i128 },
    #[error("arithmetic overflow")]
    Overflow,
    #[error("failed to parse {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse JSON {path}: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to parse CSV {path}: {source}")]
    Csv {
        path: String,
        #[source]
        source: csv::Error,
    },
}

pub type Result<T> = std::result::Result<T, SimulationError>;
