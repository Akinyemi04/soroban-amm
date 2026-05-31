use crate::error::{Result, SimulationError};
use serde::{Deserialize, Serialize};
use std::convert::TryFrom;

const BPS_DENOMINATOR: i128 = 10_000;
const PRICE_SCALE: i128 = 1_000_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PoolState {
    pub token_a: String,
    pub token_b: String,
    pub reserve_a: i128,
    pub reserve_b: i128,
    pub total_shares: i128,
    pub fee_bps: i128,
    #[serde(default)]
    pub protocol_fee_bps: i128,
    #[serde(default)]
    pub accrued_fee_a: i128,
    #[serde(default)]
    pub accrued_fee_b: i128,
    #[serde(default)]
    pub price_cumulative_a: i128,
    #[serde(default)]
    pub price_cumulative_b: i128,
    #[serde(default)]
    pub last_timestamp: u64,
    #[serde(default)]
    pub paused: bool,
}

impl PoolState {
    pub fn new(token_a: impl Into<String>, token_b: impl Into<String>, fee_bps: i128) -> Result<Self> {
        let pool = Self {
            token_a: token_a.into(),
            token_b: token_b.into(),
            reserve_a: 0,
            reserve_b: 0,
            total_shares: 0,
            fee_bps,
            protocol_fee_bps: 0,
            accrued_fee_a: 0,
            accrued_fee_b: 0,
            price_cumulative_a: 0,
            price_cumulative_b: 0,
            last_timestamp: 0,
            paused: false,
        };
        pool.validate()?;
        Ok(pool)
    }

    pub fn validate(&self) -> Result<()> {
        if self.token_a == self.token_b {
            return Err(SimulationError::InvalidToken {
                token: self.token_a.clone(),
            });
        }
        if !(0..=BPS_DENOMINATOR).contains(&self.fee_bps) {
            return Err(SimulationError::InvalidFeeBps {
                fee_bps: self.fee_bps,
            });
        }
        if !(0..=self.fee_bps).contains(&self.protocol_fee_bps) {
            return Err(SimulationError::InvalidFeeBps {
                fee_bps: self.protocol_fee_bps,
            });
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.reserve_a <= 0 || self.reserve_b <= 0
    }

    pub fn spot_price_a(&self) -> i128 {
        if self.reserve_a <= 0 || self.reserve_b <= 0 {
            0
        } else {
            self.reserve_b * PRICE_SCALE / self.reserve_a
        }
    }

    pub fn spot_price_b(&self) -> i128 {
        if self.reserve_a <= 0 || self.reserve_b <= 0 {
            0
        } else {
            self.reserve_a * PRICE_SCALE / self.reserve_b
        }
    }

    pub fn advance_to(&mut self, timestamp: u64) -> Result<()> {
        if timestamp < self.last_timestamp {
            return Err(SimulationError::InvalidInput(format!(
                "timestamps must be non-decreasing (got {timestamp}, last {})",
                self.last_timestamp
            )));
        }
        if timestamp == self.last_timestamp {
            return Ok(());
        }
        let delta = timestamp - self.last_timestamp;
        if !self.is_empty() {
            let spot_a = self.spot_price_a();
            let spot_b = self.spot_price_b();
            let delta_i128 = i128::try_from(delta).map_err(|_| SimulationError::Overflow)?;
            self.price_cumulative_a = self
                .price_cumulative_a
                .checked_add(spot_a.checked_mul(delta_i128).ok_or(SimulationError::Overflow)?)
                .ok_or(SimulationError::Overflow)?;
            self.price_cumulative_b = self
                .price_cumulative_b
                .checked_add(spot_b.checked_mul(delta_i128).ok_or(SimulationError::Overflow)?)
                .ok_or(SimulationError::Overflow)?;
        }
        self.last_timestamp = timestamp;
        Ok(())
    }

    pub fn quote_swap_exact_in(&self, token_in: &str, amount_in: i128) -> Result<SwapQuote> {
        if amount_in <= 0 {
            return Err(SimulationError::ZeroAmount);
        }
        let (reserve_in, reserve_out, token_out) = self.token_pair(token_in)?;
        if reserve_in <= 0 || reserve_out <= 0 {
            return Err(SimulationError::EmptyPool);
        }
        let amount_in_with_fee = amount_in
            .checked_mul(BPS_DENOMINATOR - self.fee_bps)
            .ok_or(SimulationError::Overflow)?;
        let numerator = amount_in_with_fee
            .checked_mul(reserve_out)
            .ok_or(SimulationError::Overflow)?;
        let denominator = reserve_in
            .checked_mul(BPS_DENOMINATOR)
            .ok_or(SimulationError::Overflow)?
            .checked_add(amount_in_with_fee)
            .ok_or(SimulationError::Overflow)?;
        let amount_out = numerator / denominator;
        let fee_amount = amount_in
            .checked_mul(self.fee_bps)
            .ok_or(SimulationError::Overflow)?
            / BPS_DENOMINATOR;
        let spot_price = reserve_out * PRICE_SCALE / reserve_in;
        let effective_price = amount_out * PRICE_SCALE / amount_in;
        let price_impact_bps = if spot_price > 0 {
            ((spot_price - effective_price) * BPS_DENOMINATOR / spot_price).max(0)
        } else {
            0
        };

        Ok(SwapQuote {
            token_in: token_in.to_string(),
            token_out,
            amount_in,
            amount_out,
            fee_amount,
            spot_price,
            effective_price,
            price_impact_bps,
            valid: amount_out > 0 && amount_out < reserve_out,
        })
    }

    pub fn quote_swap_exact_out(&self, token_out: &str, amount_out: i128) -> Result<SwapResult> {
        if amount_out <= 0 {
            return Err(SimulationError::ZeroAmount);
        }
        if self.fee_bps == BPS_DENOMINATOR {
            return Err(SimulationError::InvalidInput(
                "exact-out swaps are impossible at a 100% fee".into(),
            ));
        }
        let (reserve_in, reserve_out, token_in) = self.reverse_pair(token_out)?;
        if reserve_in <= 0 || reserve_out <= 0 {
            return Err(SimulationError::EmptyPool);
        }
        if amount_out >= reserve_out {
            return Err(SimulationError::SlippageExceeded);
        }
        let numerator = reserve_in
            .checked_mul(amount_out)
            .ok_or(SimulationError::Overflow)?
            .checked_mul(BPS_DENOMINATOR)
            .ok_or(SimulationError::Overflow)?;
        let denominator = reserve_out
            .checked_sub(amount_out)
            .ok_or(SimulationError::Overflow)?
            .checked_mul(BPS_DENOMINATOR - self.fee_bps)
            .ok_or(SimulationError::Overflow)?;
        let required_in = numerator / denominator + 1;
        let fee_amount = required_in
            .checked_mul(self.fee_bps)
            .ok_or(SimulationError::Overflow)?
            / BPS_DENOMINATOR;
        Ok(SwapResult {
            token_in,
            token_out: token_out.to_string(),
            amount_in: required_in,
            amount_out,
            fee_amount,
        })
    }

    pub fn quote_add_liquidity(&self, amount_a: i128, amount_b: i128) -> Result<LiquidityQuote> {
        if amount_a <= 0 || amount_b <= 0 {
            return Err(SimulationError::ZeroAmount);
        }
        if self.total_shares > 0 && (self.reserve_a <= 0 || self.reserve_b <= 0) {
            return Err(SimulationError::EmptyPool);
        }
        let shares = if self.total_shares == 0 {
            isqrt(amount_a.checked_mul(amount_b).ok_or(SimulationError::Overflow)?)
        } else {
            let shares_a = amount_a
                .checked_mul(self.total_shares)
                .ok_or(SimulationError::Overflow)?
                / self.reserve_a;
            let shares_b = amount_b
                .checked_mul(self.total_shares)
                .ok_or(SimulationError::Overflow)?
                / self.reserve_b;
            shares_a.min(shares_b)
        };
        Ok(LiquidityQuote {
            amount_a,
            amount_b,
            shares,
            pool_ratio: if self.reserve_a > 0 {
                self.reserve_b * PRICE_SCALE / self.reserve_a
            } else {
                0
            },
        })
    }

    pub fn quote_remove_liquidity(&self, shares: i128) -> Result<LiquidityQuote> {
        if shares <= 0 {
            return Err(SimulationError::ZeroAmount);
        }
        if self.total_shares <= 0 {
            return Err(SimulationError::EmptyPool);
        }
        Ok(LiquidityQuote {
            amount_a: shares * self.reserve_a / self.total_shares,
            amount_b: shares * self.reserve_b / self.total_shares,
            shares,
            pool_ratio: if self.reserve_a > 0 {
                self.reserve_b * PRICE_SCALE / self.reserve_a
            } else {
                0
            },
        })
    }

    pub fn execute_swap_exact_in(
        &mut self,
        token_in: &str,
        amount_in: i128,
        min_out: i128,
    ) -> Result<SwapQuote> {
        if self.paused {
            return Err(SimulationError::Paused);
        }
        let quote = self.quote_swap_exact_in(token_in, amount_in)?;
        if quote.amount_out < min_out {
            return Err(SimulationError::SlippageExceeded);
        }
        self.apply_checkpoint()?;
        let protocol_fee = self.protocol_fee(amount_in)?;

        if token_in == self.token_a {
            self.reserve_a = self.reserve_a + amount_in - protocol_fee;
            self.reserve_b = self.reserve_b - quote.amount_out;
            self.accrued_fee_a = self.accrued_fee_a + protocol_fee;
        } else {
            self.reserve_b = self.reserve_b + amount_in - protocol_fee;
            self.reserve_a = self.reserve_a - quote.amount_out;
            self.accrued_fee_b = self.accrued_fee_b + protocol_fee;
        }
        Ok(quote)
    }

    pub fn execute_swap_exact_out(
        &mut self,
        token_out: &str,
        amount_out: i128,
        max_in: i128,
    ) -> Result<SwapResult> {
        if self.paused {
            return Err(SimulationError::Paused);
        }
        let quote = self.quote_swap_exact_out(token_out, amount_out)?;
        if quote.amount_in > max_in {
            return Err(SimulationError::SlippageExceeded);
        }
        self.apply_checkpoint()?;
        let protocol_fee = self.protocol_fee(quote.amount_in)?;

        if token_out == self.token_a {
            self.reserve_b = self.reserve_b + quote.amount_in - protocol_fee;
            self.reserve_a = self.reserve_a - amount_out;
            self.accrued_fee_b = self.accrued_fee_b + protocol_fee;
        } else {
            self.reserve_a = self.reserve_a + quote.amount_in - protocol_fee;
            self.reserve_b = self.reserve_b - amount_out;
            self.accrued_fee_a = self.accrued_fee_a + protocol_fee;
        }
        Ok(quote)
    }

    pub fn execute_add_liquidity(
        &mut self,
        amount_a: i128,
        amount_b: i128,
        min_shares: i128,
    ) -> Result<LiquidityQuote> {
        if self.paused {
            return Err(SimulationError::Paused);
        }
        let quote = self.quote_add_liquidity(amount_a, amount_b)?;
        if quote.shares < min_shares {
            return Err(SimulationError::SlippageExceeded);
        }
        self.apply_checkpoint()?;
        self.reserve_a = self.reserve_a + amount_a;
        self.reserve_b = self.reserve_b + amount_b;
        self.total_shares = self.total_shares + quote.shares;
        Ok(quote)
    }

    pub fn execute_remove_liquidity(
        &mut self,
        shares: i128,
        min_a: i128,
        min_b: i128,
    ) -> Result<LiquidityQuote> {
        if self.paused {
            return Err(SimulationError::Paused);
        }
        let quote = self.quote_remove_liquidity(shares)?;
        if quote.amount_a < min_a || quote.amount_b < min_b {
            return Err(SimulationError::SlippageExceeded);
        }
        self.apply_checkpoint()?;
        self.reserve_a = self.reserve_a - quote.amount_a;
        self.reserve_b = self.reserve_b - quote.amount_b;
        self.total_shares = self.total_shares - shares;
        Ok(quote)
    }

    fn apply_checkpoint(&mut self) -> Result<()> {
        // The simulator keeps the TWAP accumulator consistent with the contract
        // by checkpointing at the current timestamp before every state change.
        self.advance_to(self.last_timestamp)
    }

    fn protocol_fee(&self, amount: i128) -> Result<i128> {
        if self.protocol_fee_bps <= 0 {
            return Ok(0);
        }
        Ok(amount
            .checked_mul(self.protocol_fee_bps)
            .ok_or(SimulationError::Overflow)?
            / BPS_DENOMINATOR)
    }

    fn token_pair(&self, token_in: &str) -> Result<(i128, i128, String)> {
        if token_in == self.token_a {
            Ok((self.reserve_a, self.reserve_b, self.token_b.clone()))
        } else if token_in == self.token_b {
            Ok((self.reserve_b, self.reserve_a, self.token_a.clone()))
        } else {
            Err(SimulationError::InvalidToken {
                token: token_in.to_string(),
            })
        }
    }

    fn reverse_pair(&self, token_out: &str) -> Result<(i128, i128, String)> {
        if token_out == self.token_a {
            Ok((self.reserve_b, self.reserve_a, self.token_b.clone()))
        } else if token_out == self.token_b {
            Ok((self.reserve_a, self.reserve_b, self.token_a.clone()))
        } else {
            Err(SimulationError::InvalidToken {
                token: token_out.to_string(),
            })
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SwapQuote {
    pub token_in: String,
    pub token_out: String,
    pub amount_in: i128,
    pub amount_out: i128,
    pub fee_amount: i128,
    pub spot_price: i128,
    pub effective_price: i128,
    pub price_impact_bps: i128,
    pub valid: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SwapResult {
    pub token_in: String,
    pub token_out: String,
    pub amount_in: i128,
    pub amount_out: i128,
    pub fee_amount: i128,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LiquidityQuote {
    pub amount_a: i128,
    pub amount_b: i128,
    pub shares: i128,
    pub pool_ratio: i128,
}

fn isqrt(n: i128) -> i128 {
    if n <= 0 {
        return 0;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}
