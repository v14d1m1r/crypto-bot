mod alerts;
mod api;
mod backtest;
mod binance;
mod config;
mod database;
#[cfg(test)]
mod deployment_tests;
mod shutdown;
mod strategy;
mod testnet;
mod trader;

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use config::{Config, ExecutionMode};
use database::Database;
use strategy::EmaCrossover;
use tracing::{error, info, warn};
use trader::{PaperTrader, TraderState};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "crypto_bot=info".into()),
        )
        .with_target(false)
        .compact()
        .init();

    let command = std::env::args().nth(1).unwrap_or_else(|| "bot".into());
    let config = Config::from_env()?;
    match command.as_str() {
        "bot" => run_bot(config).await,
        "backtest" => run_backtest(config).await,
        "serve" => run_api(config).await,
        "testnet-check" => testnet::TestnetTrader::validate(&config).await,
        "testnet-buy" => testnet::TestnetTrader::manual(&config, strategy::Signal::Buy).await,
        "testnet-sell" => testnet::TestnetTrader::manual(&config, strategy::Signal::Sell).await,
        "telegram-check" => {
            alerts::TelegramAlerter::from_config(&config)
                .send("CRUX Telegram alert test succeeded.")
                .await
        }
        _ => {
            anyhow::bail!(
                "unknown command '{command}'; use bot, backtest, serve, testnet-check, testnet-buy, testnet-sell, or telegram-check"
            )
        }
    }
}

async fn run_bot(config: Config) -> Result<()> {
    match config.mode {
        ExecutionMode::Paper => run_paper_bot(config).await,
        ExecutionMode::Testnet => run_testnet_bot(config).await,
    }
}

async fn run_paper_bot(config: Config) -> Result<()> {
    info!(symbol = %config.symbol, interval = %config.interval,
        fast_ema = config.fast_ema, slow_ema = config.slow_ema,
        starting_cash = config.starting_cash, "starting Binance paper-trading bot");

    let database = Database::connect(&config.database_url).await?;
    let history = binance::fetch_candles(&config, config.slow_ema + 2).await?;
    let mut strategy = EmaCrossover::new(config.fast_ema, config.slow_ema)?;
    strategy.seed(history.iter().map(|candle| candle.close).collect());
    let saved_state = database.load_trader_state().await?.unwrap_or(TraderState {
        cash: config.starting_cash,
        position_quantity: None,
        entry_price: None,
    });
    let mut trader = PaperTrader::from_state(
        saved_state,
        config.position_fraction,
        config.fee_rate,
        config.stop_loss,
        config.take_profit,
    )?;
    let alerts = alerts::TelegramAlerter::from_config(&config);
    if alerts.enabled()
        && let Err(error) = alerts
            .send(&format!(
                "CRUX PAPER STARTED\nSymbol: {}\nInterval: {}\nEMA: {}/{}",
                config.symbol, config.interval, config.fast_ema, config.slow_ema
            ))
            .await
    {
        warn!(%error, "Telegram paper lifecycle alert delivery failed");
    }
    let mut api_task = tokio::spawn(api::serve(database.clone(), config.clone()));
    let mut last_connection_alert_ms = None;

    loop {
        let stream_result = tokio::select! {
            result = binance::run_stream(&config, &mut strategy, &mut trader, &database, &alerts) => result,
            result = &mut api_task => return api_stopped(result),
        };
        match stream_result {
            Ok(binance::StreamEnd::Shutdown) => break,
            Ok(binance::StreamEnd::Disconnected) => {
                warn!("market stream disconnected; reconnecting in 5 seconds");
                notify_paper_connection(
                    &alerts,
                    &mut last_connection_alert_ms,
                    "CRUX PAPER CONNECTION WARNING\nMarket stream disconnected; reconnecting in 5 seconds.",
                );
            }
            Err(error) => {
                error!(%error, "market stream failed; reconnecting in 5 seconds");
                notify_paper_connection(
                    &alerts,
                    &mut last_connection_alert_ms,
                    format!("CRUX PAPER CONNECTION ERROR\n{error}\nRetrying in 5 seconds."),
                );
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    api_task.abort();
    alerts.flush().await;
    if alerts.enabled()
        && let Err(error) = alerts.send("CRUX PAPER STOPPED").await
    {
        warn!(%error, "Telegram paper shutdown alert delivery failed");
    }
    info!(
        cash = trader.cash(),
        equity = trader.equity(trader.last_price()),
        "bot stopped"
    );
    Ok(())
}

fn notify_paper_connection(
    alerts: &alerts::TelegramAlerter,
    last_alert_ms: &mut Option<i64>,
    message: impl Into<String>,
) {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default();
    if last_alert_ms.is_none_or(|last| timestamp - last >= 300_000) {
        alerts.notify(message);
        *last_alert_ms = Some(timestamp);
    }
}

async fn run_testnet_bot(config: Config) -> Result<()> {
    info!(symbol = %config.symbol, interval = %config.interval,
        fast_ema = config.fast_ema, slow_ema = config.slow_ema,
        max_order_quote = config.max_order_quote,
        max_daily_loss_quote = config.max_daily_loss_quote,
        max_entries_per_day = config.max_entries_per_day,
        max_consecutive_losses = config.max_consecutive_losses,
        "starting Binance Spot Testnet bot");
    let database = Database::connect(&config.database_url).await?;
    let history = binance::fetch_candles(&config, config.slow_ema + 2).await?;
    let mut strategy = EmaCrossover::new(config.fast_ema, config.slow_ema)?;
    strategy.seed(history.iter().map(|candle| candle.close).collect());
    let mut trader = testnet::TestnetTrader::initialize(&config, &database).await?;
    trader
        .send_alert(&format!(
            "CRUX TESTNET STARTED\nSymbol: {}\nInterval: {}\nEMA: {}/{}",
            config.symbol, config.interval, config.fast_ema, config.slow_ema
        ))
        .await;
    let mut api_task = tokio::spawn(api::serve(database.clone(), config.clone()));
    loop {
        let stream_result = tokio::select! {
            result = testnet::run_stream(&config, &mut strategy, &mut trader, &database) => result,
            result = &mut api_task => return api_stopped(result),
        };
        match stream_result {
            Ok(binance::StreamEnd::Shutdown) => break,
            Ok(binance::StreamEnd::Disconnected) => {
                warn!("Testnet stream disconnected; reconnecting in 5 seconds");
                trader.notify_reconnect(
                    "CRUX TESTNET CONNECTION WARNING\nStream disconnected; reconnecting in 5 seconds.",
                );
            }
            Err(error) => {
                error!(%error, "Testnet stream failed; reconnecting in 5 seconds");
                trader.notify_reconnect(format!(
                    "CRUX TESTNET CONNECTION ERROR\n{error}\nRetrying in 5 seconds."
                ));
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    api_task.abort();
    trader.flush_alerts().await;
    trader.send_alert("CRUX TESTNET STOPPED").await;
    info!("Binance Spot Testnet bot stopped");
    Ok(())
}

fn api_stopped(result: Result<Result<()>, tokio::task::JoinError>) -> Result<()> {
    match result {
        Ok(Ok(())) => Err(anyhow!("dashboard API stopped unexpectedly")),
        Ok(Err(error)) => Err(error).context("dashboard API failed"),
        Err(error) => Err(error).context("dashboard API task failed"),
    }
}

async fn run_backtest(config: Config) -> Result<()> {
    let candles = binance::fetch_candles(&config, config.backtest_limit).await?;
    let report = backtest::run(&config, &candles)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

async fn run_api(config: Config) -> Result<()> {
    let database = Database::connect(&config.database_url).await?;
    api::serve(database, config).await
}
