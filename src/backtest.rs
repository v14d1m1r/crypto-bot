use anyhow::Result;
use serde::Serialize;

use crate::{binance::Candle, config::Config, strategy::EmaCrossover, trader::PaperTrader};

#[derive(Debug, Serialize)]
pub struct BacktestReport {
    pub symbol: String,
    pub interval: String,
    pub candles: usize,
    pub fast_ema: usize,
    pub slow_ema: usize,
    pub starting_cash: f64,
    pub ending_equity: f64,
    pub return_percent: f64,
    pub max_drawdown_percent: f64,
    pub trades: usize,
    pub closed_positions: usize,
    pub winning_positions: usize,
    pub win_rate_percent: f64,
}

pub fn run(config: &Config, candles: &[Candle]) -> Result<BacktestReport> {
    let mut strategy = EmaCrossover::new(config.fast_ema, config.slow_ema)?;
    let mut trader = PaperTrader::new(
        config.starting_cash,
        config.position_fraction,
        config.fee_rate,
        config.stop_loss,
        config.take_profit,
    )?;
    let mut peak = config.starting_cash;
    let mut max_drawdown: f64 = 0.0;
    let mut trades = 0;
    let mut closed_positions = 0;
    let mut winning_positions = 0;

    for candle in candles {
        let signal = strategy.on_close(candle.close);
        if let Some(trade) = trader.on_candle(candle.close, signal, candle.close_time) {
            trades += 1;
            if trade.side == "SELL" {
                closed_positions += 1;
                if trade.realized_pnl > 0.0 {
                    winning_positions += 1;
                }
            }
        }
        let equity = trader.equity(candle.close);
        peak = peak.max(equity);
        max_drawdown = max_drawdown.max((peak - equity) / peak);
    }

    let ending_equity = candles
        .last()
        .map_or(config.starting_cash, |c| trader.equity(c.close));
    Ok(BacktestReport {
        symbol: config.symbol.clone(),
        interval: config.interval.clone(),
        candles: candles.len(),
        fast_ema: config.fast_ema,
        slow_ema: config.slow_ema,
        starting_cash: config.starting_cash,
        ending_equity,
        return_percent: (ending_equity / config.starting_cash - 1.0) * 100.0,
        max_drawdown_percent: max_drawdown * 100.0,
        trades,
        closed_positions,
        winning_positions,
        win_rate_percent: if closed_positions == 0 {
            0.0
        } else {
            winning_positions as f64 / closed_positions as f64 * 100.0
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backtest_produces_finite_metrics() {
        let config = Config::default_for_test();
        let candles = (0..100)
            .map(|i| Candle {
                close_time: i,
                close: 100.0 + (i as f64 / 5.0).sin() * 10.0,
            })
            .collect::<Vec<_>>();
        let report = run(&config, &candles).unwrap();
        assert!(report.ending_equity.is_finite());
        assert!(report.max_drawdown_percent >= 0.0);
    }
}
