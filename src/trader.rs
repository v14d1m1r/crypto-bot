use anyhow::{Result, bail};
use serde::Serialize;
use tracing::info;

use crate::strategy::Signal;

#[derive(Debug, Clone, Copy)]
struct Position {
    quantity: f64,
    entry_price: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Trade {
    pub timestamp: i64,
    pub side: &'static str,
    pub price: f64,
    pub quantity: f64,
    pub fee: f64,
    pub realized_pnl: f64,
    pub reason: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct TraderState {
    pub cash: f64,
    pub position_quantity: Option<f64>,
    pub entry_price: Option<f64>,
}

pub struct PaperTrader {
    cash: f64,
    position: Option<Position>,
    position_fraction: f64,
    fee_rate: f64,
    stop_loss: f64,
    take_profit: f64,
    last_price: f64,
}

impl PaperTrader {
    pub fn new(
        cash: f64,
        position_fraction: f64,
        fee_rate: f64,
        stop_loss: f64,
        take_profit: f64,
    ) -> Result<Self> {
        if cash <= 0.0 || !(0.0..1.0).contains(&position_fraction) {
            bail!("invalid paper trader cash or position fraction");
        }
        Ok(Self {
            cash,
            position: None,
            position_fraction,
            fee_rate,
            stop_loss,
            take_profit,
            last_price: 0.0,
        })
    }

    pub fn from_state(
        state: TraderState,
        position_fraction: f64,
        fee_rate: f64,
        stop_loss: f64,
        take_profit: f64,
    ) -> Result<Self> {
        let mut trader = Self::new(
            state.cash,
            position_fraction,
            fee_rate,
            stop_loss,
            take_profit,
        )?;
        trader.position = match (state.position_quantity, state.entry_price) {
            (Some(quantity), Some(entry_price)) => Some(Position {
                quantity,
                entry_price,
            }),
            _ => None,
        };
        Ok(trader)
    }

    pub fn on_candle(
        &mut self,
        price: f64,
        signal: Option<Signal>,
        timestamp: i64,
    ) -> Option<Trade> {
        self.last_price = price;
        if let Some(position) = self.position {
            let change = price / position.entry_price - 1.0;
            if change <= -self.stop_loss {
                return self.sell(price, "stop loss", timestamp);
            }
            if change >= self.take_profit {
                return self.sell(price, "take profit", timestamp);
            }
        }
        match signal {
            Some(Signal::Buy) if self.position.is_none() => Some(self.buy(price, timestamp)),
            Some(Signal::Sell) if self.position.is_some() => {
                self.sell(price, "EMA crossover", timestamp)
            }
            _ => None,
        }
    }

    fn buy(&mut self, price: f64, timestamp: i64) -> Trade {
        let budget = self.cash * self.position_fraction;
        let quantity = budget * (1.0 - self.fee_rate) / price;
        let fee = budget * self.fee_rate;
        self.cash -= budget;
        self.position = Some(Position {
            quantity,
            entry_price: price,
        });
        info!(
            side = "BUY",
            price,
            quantity,
            fee,
            cash = self.cash,
            "paper order filled"
        );
        Trade {
            timestamp,
            side: "BUY",
            price,
            quantity,
            fee,
            realized_pnl: 0.0,
            reason: "EMA crossover",
        }
    }

    fn sell(&mut self, price: f64, reason: &'static str, timestamp: i64) -> Option<Trade> {
        if let Some(position) = self.position.take() {
            let gross = position.quantity * price;
            let fee = gross * self.fee_rate;
            let proceeds = gross * (1.0 - self.fee_rate);
            let pnl = proceeds - position.quantity * position.entry_price;
            self.cash += proceeds;
            info!(
                side = "SELL",
                price,
                quantity = position.quantity,
                fee,
                pnl,
                cash = self.cash,
                reason,
                "paper order filled"
            );
            return Some(Trade {
                timestamp,
                side: "SELL",
                price,
                quantity: position.quantity,
                fee,
                realized_pnl: pnl,
                reason,
            });
        }
        None
    }

    pub fn cash(&self) -> f64 {
        self.cash
    }
    pub fn last_price(&self) -> f64 {
        self.last_price
    }
    pub fn equity(&self, price: f64) -> f64 {
        self.cash
            + self
                .position
                .map_or(0.0, |position| position.quantity * price)
    }

    pub fn state(&self) -> TraderState {
        TraderState {
            cash: self.cash,
            position_quantity: self.position.map(|position| position.quantity),
            entry_price: self.position.map(|position| position.entry_price),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_loss_closes_position() {
        let mut trader = PaperTrader::new(1_000.0, 0.5, 0.0, 0.02, 0.04).unwrap();
        trader.on_candle(100.0, Some(Signal::Buy), 1);
        assert_eq!(trader.cash(), 500.0);
        trader.on_candle(97.0, None, 2);
        assert_eq!(trader.equity(97.0), trader.cash());
        assert!(trader.cash() < 1_000.0);
    }
}
