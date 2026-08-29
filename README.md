# Rust Binance EMA paper-trading system

A local BTC/USDT trading workspace with Binance candle data, an EMA 20/50 strategy, paper execution, Binance Spot Testnet execution, SQLite persistence, a historical backtester, a Rust JSON API, and a React dashboard. Production Binance trading is not implemented.

## Architecture

```text
Binance REST + WebSocket
        ↓
closed 1m/5m candles
        ↓
shared EMA 20/50 engine
        ↓
paper trader + risk controls
        ↓
SQLite ← Rust API ← React dashboard
        ↘ backtester
```

Paper trading, Testnet execution, and backtesting share the same EMA strategy. Trades close on an EMA bearish crossover, 2% stop loss, or 4% take profit. Paper mode includes a configurable simulated fee; Testnet mode recovers exact per-fill prices, quantities, and native commission assets from Binance account trade history.

## Run locally

Requirements: Rust/Cargo, Node.js 22.13 or newer, npm, and outbound access to Binance.

Open PowerShell in this directory and start the bot/API:

```powershell
cargo test
cargo run -- bot
```

In a second PowerShell window, start the dashboard:

```powershell
cd dashboard
npm.cmd install
npm.cmd run dev
```

Open `http://localhost:3000`. The Rust API listens on `http://127.0.0.1:3001`. Stop either process with `Ctrl+C`.

SQLite data is stored in `crypto_bot.db`. Restarting the bot restores paper cash and an open position. Delete or rename that file only when you intentionally want a fresh paper account.

## Backtest

Run the strategy against the latest Binance candles:

```powershell
cargo run -- backtest
```

The command prints JSON containing return, ending equity, maximum drawdown, trade count, and win rate. Binance's single kline request is limited to 1,000 candles, so `BOT_BACKTEST_LIMIT` must be at most 1,000.

## Automated fault/recovery harness

Run the deterministic Testnet recovery scenarios without Binance credentials or external network access:

```powershell
cargo test fault_harness -- --nocapture
```

The harness starts an ephemeral local mock exchange and exercises the production reconciliation code. It verifies recovery of missed protective fills without duplicate trades, rejection of duplicate execution reports, one-time OCO restoration after reconnect, retry after a transient REST failure, reconstruction of a buy filled immediately before a crash, recovery when a trade committed before final position state, and safe clearing of stale local state after a Testnet account reset.

## Configuration

PowerShell does not automatically load `.env.example`. Set variables in the terminal before starting the process:

```powershell
$env:BOT_SYMBOL = "BTCUSDT"
$env:BOT_INTERVAL = "5m"
$env:BOT_FAST_EMA = "20"
$env:BOT_SLOW_EMA = "50"
$env:BOT_POSITION_FRACTION = "0.25"
$env:BOT_BACKTEST_LIMIT = "1000"
cargo run -- bot
```

See `.env.example` for every setting. Run only the API over existing SQLite data with `cargo run -- serve`. Set `NEXT_PUBLIC_API_URL` before building the dashboard if the API is not at its default address.

## Safety boundary

Production trading remains disabled. Testnet now covers signing, filters, market execution, unique client order IDs, exact account-trade fill recovery, signed user-data events, reconnect reconciliation, exchange-hosted protective orders, and persistent UTC-day entry circuit breakers. Before adding production execution, it still needs longer fault testing, operational alerting, and explicit production-only safety review. Keep withdrawal permission disabled on every production trading key.

This software does not make the EMA strategy profitable and is not financial advice.

## Binance Spot Testnet

Spot Testnet uses simulated assets and separate credentials. Create the keys at `https://testnet.binance.vision/`; do not use keys from your production Binance account and never commit secrets to this repository.

Set credentials only in the current PowerShell session:

```powershell
$env:BOT_MODE = "testnet"
$env:BINANCE_TESTNET_API_KEY = "your-testnet-api-key"
$env:BINANCE_TESTNET_SECRET_KEY = "your-testnet-secret"
$env:BOT_MAX_ORDER_QUOTE = "25"
$env:BOT_MAX_DAILY_LOSS_QUOTE = "10"
$env:BOT_MAX_ENTRIES_PER_DAY = "10"
$env:BOT_MAX_CONSECUTIVE_LOSSES = "3"
```

First validate authentication, balances, symbol filters, signing, and a non-executing Binance test order:

```powershell
cargo run -- testnet-check
```

The command must finish successfully before starting execution:

```powershell
cargo run -- bot
```

In Testnet mode the bot:

- hard-locks REST and market data to Binance Spot Testnet hosts;
- uses the separate `crypto_bot_testnet.db` database;
- caps each buy at `BOT_MAX_ORDER_QUOTE` USDT and the configured cash fraction;
- submits `MARKET` buys with `quoteOrderQty` and rounded `MARKET` sells;
- records Binance order IDs, client order IDs, fills, trades, and account equity;
- exposes the latest exact Binance fills at `GET /api/fills`;
- queries an order by its unique client ID instead of blindly retrying an uncertain request;
- tracks only BTC acquired by this bot, rather than selling unrelated account BTC;
- reconciles balances, recent orders, open protective orders, and persisted state on startup and after every WebSocket reconnection;
- subscribes to signed user-data execution reports;
- installs a Binance-hosted OCO stop-loss/take-profit after each entry;
- cancels the OCO before an EMA strategy exit.
- persistently blocks new entries after the configured UTC-day loss, entry-count, or consecutive-loss limit is reached, while continuing to allow exits.

Every Binance fill is stored once by its exchange trade ID, including its native commission amount and asset. The summarized `trades.fee` value is exact in USDT when commission is charged in USDT, and uses the fill price when commission is charged in BTC. A commission paid in a third asset such as BNB remains exact in `/api/fills` but is not converted to a historical USDT value.

Risk status and configured limits are returned by `GET /api/status`. Once triggered, the Testnet circuit breaker stays halted in SQLite for the rest of that UTC day even if the process restarts. It resets automatically on the first evaluation in the next UTC day. The breaker never prevents an existing position from being closed.

### Telegram operational alerts

Create a bot with Telegram's `@BotFather`, open the new bot chat, and send `/start`. Configure the token only in the current PowerShell session:

```powershell
$env:TELEGRAM_BOT_TOKEN = "your-bot-token"
```

After sending `/start`, retrieve the chat ID from the latest bot update and keep it in the current session:

```powershell
$updates = Invoke-RestMethod -Uri "https://api.telegram.org/bot$($env:TELEGRAM_BOT_TOKEN)/getUpdates"
$env:TELEGRAM_CHAT_ID = [string]$updates.result[-1].message.chat.id
```

Send a deliberate test message before starting the bot:

```powershell
cargo run -- telegram-check
```

When enabled, both paper and Testnet modes send alerts for startup, shutdown, and executed buys and sells. Testnet additionally reports OCO installation or failure, emergency flattening, risk-circuit activation, connection failures, and successful stream recovery. Routine delivery is non-blocking and best-effort: a Telegram failure is logged but cannot stop trading or protective exits. Repeated connection-failure alerts are limited to one every five minutes. Short-lived manual Testnet commands flush their queued execution alerts before exiting.

Never commit the Telegram token or paste it into chat, logs, screenshots, or source files. Remove the session variables when finished:

```powershell
Remove-Item Env:TELEGRAM_BOT_TOKEN
Remove-Item Env:TELEGRAM_CHAT_ID
```

### Guarded manual Testnet cycle

Use these commands only to verify a complete Testnet lifecycle without waiting for an EMA crossover. They submit actual orders against simulated Testnet assets. Stop the continuously running bot first so only one process controls the account.

```powershell
$env:BOT_MANUAL_CONFIRM = "TESTNET_ONLY"
cargo run -- testnet-buy
```

Confirm that the market buy and its protective OCO appear in Testnet and the dashboard. Then test the exit; it cancels the OCO before submitting the market sell:

```powershell
cargo run -- testnet-sell
Remove-Item Env:BOT_MANUAL_CONFIRM
```

Manual buy refuses an existing tracked position, and manual sell refuses an empty tracked position. Never leave the confirmation variable enabled after testing.

The Testnet account can be reset by Binance, including balances and orders. If that happens, stop the bot, remove the local Testnet database intentionally, rerun `testnet-check`, and start again. Paper mode remains the default whenever `BOT_MODE` is unset.
