use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use crate::{
    alerts::TelegramAlerter, config::Config, database::Database, strategy::EmaCrossover,
    trader::PaperTrader,
};

#[derive(Debug, Clone)]
pub struct Candle {
    pub close_time: i64,
    pub close: f64,
}

#[derive(Debug, Deserialize)]
struct KlineEvent {
    #[serde(rename = "k")]
    kline: Kline,
}

#[derive(Debug, Deserialize)]
struct Kline {
    #[serde(rename = "c")]
    close: String,
    #[serde(rename = "x")]
    closed: bool,
    #[serde(rename = "T")]
    close_time: u64,
}

pub fn parse_closed_candle(text: &str) -> Result<Option<Candle>> {
    let event: KlineEvent = serde_json::from_str(text).context("invalid Binance kline event")?;
    if !event.kline.closed {
        return Ok(None);
    }
    Ok(Some(Candle {
        close: event
            .kline
            .close
            .parse::<f64>()
            .context("invalid streamed close price")?,
        close_time: event.kline.close_time as i64,
    }))
}

pub fn market_stream_url(config: &Config) -> String {
    let stream = format!("{}@kline_{}", config.symbol.to_lowercase(), config.interval);
    format!("{}/{}", config.ws_base.trim_end_matches('/'), stream)
}

pub enum StreamEnd {
    Shutdown,
    Disconnected,
}

pub async fn fetch_candles(config: &Config, limit: usize) -> Result<Vec<Candle>> {
    let url = format!("{}/api/v3/klines", config.rest_base.trim_end_matches('/'));
    let limit = limit.to_string();
    let response = reqwest::Client::new()
        .get(url)
        .query(&[
            ("symbol", config.symbol.as_str()),
            ("interval", config.interval.as_str()),
            ("limit", limit.as_str()),
        ])
        .send()
        .await
        .context("failed to request Binance kline history")?
        .error_for_status()
        .context("Binance rejected the kline history request")?;
    let rows: Vec<Vec<Value>> = response
        .json()
        .await
        .context("invalid Binance kline history response")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as i64;
    let candles = rows
        .into_iter()
        .map(|row| {
            let close = row
                .get(4)
                .and_then(Value::as_str)
                .context("kline has no close price")?
                .parse::<f64>()
                .context("invalid kline close price")?;
            let close_time = row
                .get(6)
                .and_then(Value::as_i64)
                .context("kline has no close time")?;
            Ok(Candle { close_time, close })
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|candle| candle.close_time <= now)
        .collect::<Vec<_>>();
    if candles.len() < config.slow_ema {
        bail!(
            "Binance returned only {} candles; need {}",
            candles.len(),
            config.slow_ema
        );
    }
    info!(candles = candles.len(), "loaded historical candles");
    Ok(candles)
}

pub async fn run_stream(
    config: &Config,
    strategy: &mut EmaCrossover,
    trader: &mut PaperTrader,
    database: &Database,
    alerts: &TelegramAlerter,
) -> Result<StreamEnd> {
    let url = market_stream_url(config);
    let (socket, _) = connect_async(&url)
        .await
        .with_context(|| format!("failed to connect to {url}"))?;
    info!(%url, "market stream connected");
    let (mut writer, mut reader) = socket.split();

    loop {
        tokio::select! {
            _ = crate::shutdown::signal() => {
                let _ = writer.close().await;
                return Ok(StreamEnd::Shutdown);
            }
            message = reader.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    if let Some(candle) = parse_closed_candle(&text)? {
                        let price = candle.close;
                        let signal = strategy.on_close(price);
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)?
                            .as_millis() as i64;
                        let stale_entry = config.candle_is_stale(candle.close_time, now)
                            && signal == Some(crate::strategy::Signal::Buy)
                            && trader.state().position_quantity.is_none();
                        let signal = if stale_entry {
                            warn!(close_time = candle.close_time, stale_after_seconds = config.stale_data_seconds,
                                "stale paper candle cannot open a new position");
                            None
                        } else {
                            signal
                        };
                        let (fast, slow) = strategy.averages().unwrap_or_default();
                        debug!(price, fast_ema = fast, slow_ema = slow, close_time = candle.close_time, "candle closed");
                        let trade = trader.on_candle(price, signal, candle.close_time);
                        let equity = trader.equity(price);
                        database.record_candle(&config.symbol, &config.interval, &candle).await?;
                        if let Some(trade) = &trade {
                            database.record_trade(&config.symbol, trade).await?;
                            alerts.notify(format!(
                                "CRUX PAPER {}\n{}\nPrice: {:.8}\nQuantity: {:.8}\nFee: {:.8}\nRealized PnL: {:.8}",
                                trade.side, trade.reason, trade.price, trade.quantity, trade.fee,
                                trade.realized_pnl
                            ));
                        }
                        database.save_state(candle.close_time, trader.state(), price, equity).await?;
                        info!(price, fast_ema = fast, slow_ema = slow, equity, ?signal, "strategy evaluated");
                    }
                }
                Some(Ok(Message::Ping(payload))) => writer.send(Message::Pong(payload)).await.context("failed to answer Binance ping")?,
                Some(Ok(Message::Close(_))) | None => return Ok(StreamEnd::Disconnected),
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error).context("Binance WebSocket error"),
            }
        }
    }
}
