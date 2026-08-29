use anyhow::{Context, Result};
use serde::Serialize;
use sqlx::{FromRow, SqlitePool, sqlite::SqlitePoolOptions};

use crate::{
    binance::Candle,
    trader::{Trade, TraderState},
};

#[derive(Clone)]
pub struct Database {
    pool: SqlitePool,
}

#[derive(Debug, Serialize, FromRow)]
pub struct TradeRow {
    pub id: i64,
    pub timestamp: i64,
    pub symbol: String,
    pub side: String,
    pub price: f64,
    pub quantity: f64,
    pub fee: f64,
    pub realized_pnl: f64,
    pub reason: String,
}

#[derive(Debug, Serialize, FromRow)]
pub struct EquityRow {
    pub timestamp: i64,
    pub cash: f64,
    pub equity: f64,
    pub position_quantity: Option<f64>,
    pub price: f64,
}

#[derive(Debug, Serialize, FromRow)]
pub struct CandleRow {
    pub close_time: i64,
    pub close: f64,
}

#[derive(Debug, Serialize, FromRow)]
pub struct ExchangeFillRow {
    pub trade_id: i64,
    pub order_id: i64,
    pub symbol: String,
    pub price: f64,
    pub quantity: f64,
    pub quote_quantity: f64,
    pub commission: f64,
    pub commission_asset: String,
    pub timestamp: i64,
}

#[derive(Debug, Serialize, FromRow)]
pub struct StatusRow {
    pub cash: f64,
    pub position_quantity: Option<f64>,
    pub entry_price: Option<f64>,
    pub last_price: f64,
    pub equity: f64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct RiskStatusRow {
    pub environment: String,
    pub symbol: String,
    pub utc_day_start: i64,
    pub halted: bool,
    pub reason: Option<String>,
    pub daily_realized_pnl: f64,
    pub entries_today: i64,
    pub consecutive_losses: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy)]
pub struct RiskLimits {
    pub max_daily_loss_quote: f64,
    pub max_entries_per_day: i64,
    pub max_consecutive_losses: i64,
}

impl Database {
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(url)
            .await
            .with_context(|| format!("failed to open SQLite database at {url}"))?;
        let database = Self { pool };
        database.migrate().await?;
        Ok(database)
    }

    async fn migrate(&self) -> Result<()> {
        sqlx::query("CREATE TABLE IF NOT EXISTS candles (id INTEGER PRIMARY KEY, symbol TEXT NOT NULL, interval TEXT NOT NULL, close_time INTEGER NOT NULL, close REAL NOT NULL, UNIQUE(symbol, interval, close_time))")
            .execute(&self.pool).await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_candles_symbol_interval_time ON candles(symbol, interval, close_time DESC)")
            .execute(&self.pool).await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS trades (id INTEGER PRIMARY KEY, timestamp INTEGER NOT NULL, symbol TEXT NOT NULL, side TEXT NOT NULL, price REAL NOT NULL, quantity REAL NOT NULL, fee REAL NOT NULL, realized_pnl REAL NOT NULL, reason TEXT NOT NULL)")
            .execute(&self.pool).await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_trades_symbol_time ON trades(symbol, timestamp DESC)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS equity_snapshots (id INTEGER PRIMARY KEY, timestamp INTEGER NOT NULL, cash REAL NOT NULL, equity REAL NOT NULL, position_quantity REAL, price REAL NOT NULL)")
            .execute(&self.pool).await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_equity_time ON equity_snapshots(timestamp DESC)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS bot_state (id INTEGER PRIMARY KEY CHECK(id = 1), cash REAL NOT NULL, position_quantity REAL, entry_price REAL, last_price REAL NOT NULL, equity REAL NOT NULL, updated_at INTEGER NOT NULL)")
            .execute(&self.pool).await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS exchange_orders (id INTEGER PRIMARY KEY, environment TEXT NOT NULL, symbol TEXT NOT NULL, exchange_order_id INTEGER NOT NULL, client_order_id TEXT NOT NULL, side TEXT NOT NULL, status TEXT NOT NULL, executed_quantity REAL NOT NULL, quote_quantity REAL NOT NULL, timestamp INTEGER NOT NULL, UNIQUE(environment, exchange_order_id), UNIQUE(environment, client_order_id))")
            .execute(&self.pool).await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_exchange_orders_symbol_time ON exchange_orders(environment, symbol, timestamp DESC)")
            .execute(&self.pool).await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS exchange_fills (id INTEGER PRIMARY KEY, environment TEXT NOT NULL, symbol TEXT NOT NULL, trade_id INTEGER NOT NULL, exchange_order_id INTEGER NOT NULL, price REAL NOT NULL, quantity REAL NOT NULL, quote_quantity REAL NOT NULL, commission REAL NOT NULL, commission_asset TEXT NOT NULL, timestamp INTEGER NOT NULL, UNIQUE(environment, symbol, trade_id))")
            .execute(&self.pool).await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_exchange_fills_order ON exchange_fills(environment, symbol, exchange_order_id)")
            .execute(&self.pool).await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS exchange_trade_records (id INTEGER PRIMARY KEY, environment TEXT NOT NULL, symbol TEXT NOT NULL, exchange_order_id INTEGER NOT NULL, UNIQUE(environment, symbol, exchange_order_id))")
            .execute(&self.pool).await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS risk_state (id INTEGER PRIMARY KEY, environment TEXT NOT NULL, symbol TEXT NOT NULL, utc_day_start INTEGER NOT NULL, halted INTEGER NOT NULL, reason TEXT, daily_realized_pnl REAL NOT NULL, entries_today INTEGER NOT NULL, consecutive_losses INTEGER NOT NULL, updated_at INTEGER NOT NULL, UNIQUE(environment, symbol))")
            .execute(&self.pool).await?;
        sqlx::query("PRAGMA optimize").execute(&self.pool).await?;
        Ok(())
    }

    pub async fn record_candle(&self, symbol: &str, interval: &str, candle: &Candle) -> Result<()> {
        sqlx::query("INSERT INTO candles(symbol, interval, close_time, close) VALUES (?, ?, ?, ?) ON CONFLICT(symbol, interval, close_time) DO UPDATE SET close = excluded.close")
            .bind(symbol).bind(interval).bind(candle.close_time).bind(candle.close)
            .execute(&self.pool).await?;
        Ok(())
    }

    pub async fn record_trade(&self, symbol: &str, trade: &Trade) -> Result<()> {
        sqlx::query("INSERT INTO trades(timestamp, symbol, side, price, quantity, fee, realized_pnl, reason) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(trade.timestamp).bind(symbol).bind(trade.side).bind(trade.price)
            .bind(trade.quantity).bind(trade.fee).bind(trade.realized_pnl).bind(trade.reason)
            .execute(&self.pool).await?;
        Ok(())
    }

    pub async fn record_exchange_trade_once(
        &self,
        environment: &str,
        symbol: &str,
        exchange_order_id: i64,
        trade: &Trade,
    ) -> Result<bool> {
        let mut transaction = self.pool.begin().await?;
        let inserted = sqlx::query("INSERT OR IGNORE INTO exchange_trade_records(environment, symbol, exchange_order_id) VALUES (?, ?, ?)")
            .bind(environment).bind(symbol).bind(exchange_order_id)
            .execute(&mut *transaction).await?.rows_affected() == 1;
        if inserted {
            sqlx::query("INSERT INTO trades(timestamp, symbol, side, price, quantity, fee, realized_pnl, reason) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
                .bind(trade.timestamp).bind(symbol).bind(trade.side).bind(trade.price)
                .bind(trade.quantity).bind(trade.fee).bind(trade.realized_pnl).bind(trade.reason)
                .execute(&mut *transaction).await?;
        }
        transaction.commit().await?;
        Ok(inserted)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn record_exchange_order(
        &self,
        environment: &str,
        symbol: &str,
        exchange_order_id: i64,
        client_order_id: &str,
        side: &str,
        status: &str,
        executed_quantity: f64,
        quote_quantity: f64,
        timestamp: i64,
    ) -> Result<()> {
        sqlx::query("INSERT INTO exchange_orders(environment, symbol, exchange_order_id, client_order_id, side, status, executed_quantity, quote_quantity, timestamp) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(environment, exchange_order_id) DO UPDATE SET status=excluded.status, executed_quantity=excluded.executed_quantity, quote_quantity=excluded.quote_quantity")
            .bind(environment).bind(symbol).bind(exchange_order_id).bind(client_order_id)
            .bind(side).bind(status).bind(executed_quantity).bind(quote_quantity).bind(timestamp)
            .execute(&self.pool).await?;
        Ok(())
    }

    pub async fn exchange_order_status(
        &self,
        environment: &str,
        exchange_order_id: i64,
    ) -> Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT status FROM exchange_orders WHERE environment=? AND exchange_order_id=?",
        )
        .bind(environment)
        .bind(exchange_order_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn record_exchange_fill(
        &self,
        environment: &str,
        symbol: &str,
        trade_id: i64,
        exchange_order_id: i64,
        price: f64,
        quantity: f64,
        quote_quantity: f64,
        commission: f64,
        commission_asset: &str,
        timestamp: i64,
    ) -> Result<()> {
        sqlx::query("INSERT INTO exchange_fills(environment, symbol, trade_id, exchange_order_id, price, quantity, quote_quantity, commission, commission_asset, timestamp) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(environment, symbol, trade_id) DO UPDATE SET exchange_order_id=excluded.exchange_order_id, price=excluded.price, quantity=excluded.quantity, quote_quantity=excluded.quote_quantity, commission=excluded.commission, commission_asset=excluded.commission_asset, timestamp=excluded.timestamp")
            .bind(environment).bind(symbol).bind(trade_id).bind(exchange_order_id)
            .bind(price).bind(quantity).bind(quote_quantity).bind(commission)
            .bind(commission_asset).bind(timestamp).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn recent_exchange_fills(&self, limit: i64) -> Result<Vec<ExchangeFillRow>> {
        Ok(sqlx::query_as("SELECT trade_id, exchange_order_id AS order_id, symbol, price, quantity, quote_quantity, commission, commission_asset, timestamp FROM exchange_fills ORDER BY timestamp DESC, trade_id DESC LIMIT ?")
            .bind(limit).fetch_all(&self.pool).await?)
    }

    pub async fn evaluate_risk(
        &self,
        environment: &str,
        symbol: &str,
        timestamp: i64,
        limits: RiskLimits,
    ) -> Result<RiskStatusRow> {
        const UTC_DAY_MS: i64 = 86_400_000;
        let utc_day_start = timestamp.div_euclid(UTC_DAY_MS) * UTC_DAY_MS;
        let daily_realized_pnl: f64 = sqlx::query_scalar("SELECT COALESCE(SUM(realized_pnl), 0.0) FROM trades WHERE symbol=? AND side='SELL' AND timestamp>=? AND timestamp<?")
            .bind(symbol).bind(utc_day_start).bind(utc_day_start + UTC_DAY_MS)
            .fetch_one(&self.pool).await?;
        let entries_today: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM trades WHERE symbol=? AND side='BUY' AND timestamp>=? AND timestamp<?")
            .bind(symbol).bind(utc_day_start).bind(utc_day_start + UTC_DAY_MS)
            .fetch_one(&self.pool).await?;
        let recent_exits: Vec<f64> = sqlx::query_scalar("SELECT realized_pnl FROM trades WHERE symbol=? AND side='SELL' AND timestamp>=? AND timestamp<? ORDER BY timestamp DESC, id DESC")
            .bind(symbol).bind(utc_day_start).bind(utc_day_start + UTC_DAY_MS)
            .fetch_all(&self.pool).await?;
        let consecutive_losses = recent_exits.iter().take_while(|pnl| **pnl < 0.0).count() as i64;
        let previous = self.risk_status(environment, symbol).await?;
        let persistent_reason = previous
            .filter(|state| state.utc_day_start == utc_day_start && state.halted)
            .and_then(|state| state.reason);
        let reason = persistent_reason.or_else(|| {
            if daily_realized_pnl <= -limits.max_daily_loss_quote {
                Some(format!(
                    "daily realized PnL {daily_realized_pnl:.8} reached loss limit -{:.8}",
                    limits.max_daily_loss_quote
                ))
            } else if entries_today >= limits.max_entries_per_day {
                Some(format!(
                    "daily entry limit reached: {entries_today}/{}",
                    limits.max_entries_per_day
                ))
            } else if consecutive_losses >= limits.max_consecutive_losses {
                Some(format!(
                    "consecutive loss limit reached: {consecutive_losses}/{}",
                    limits.max_consecutive_losses
                ))
            } else {
                None
            }
        });
        let state = RiskStatusRow {
            environment: environment.into(),
            symbol: symbol.into(),
            utc_day_start,
            halted: reason.is_some(),
            reason,
            daily_realized_pnl,
            entries_today,
            consecutive_losses,
            updated_at: timestamp,
        };
        sqlx::query("INSERT INTO risk_state(environment, symbol, utc_day_start, halted, reason, daily_realized_pnl, entries_today, consecutive_losses, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(environment, symbol) DO UPDATE SET utc_day_start=excluded.utc_day_start, halted=excluded.halted, reason=excluded.reason, daily_realized_pnl=excluded.daily_realized_pnl, entries_today=excluded.entries_today, consecutive_losses=excluded.consecutive_losses, updated_at=excluded.updated_at")
            .bind(&state.environment).bind(&state.symbol).bind(state.utc_day_start)
            .bind(state.halted).bind(&state.reason).bind(state.daily_realized_pnl)
            .bind(state.entries_today).bind(state.consecutive_losses).bind(state.updated_at)
            .execute(&self.pool).await?;
        Ok(state)
    }

    pub async fn risk_status(
        &self,
        environment: &str,
        symbol: &str,
    ) -> Result<Option<RiskStatusRow>> {
        Ok(sqlx::query_as("SELECT environment, symbol, utc_day_start, halted, reason, daily_realized_pnl, entries_today, consecutive_losses, updated_at FROM risk_state WHERE environment=? AND symbol=?")
            .bind(environment).bind(symbol).fetch_optional(&self.pool).await?)
    }

    pub async fn save_state(
        &self,
        timestamp: i64,
        state: TraderState,
        last_price: f64,
        equity: f64,
    ) -> Result<()> {
        sqlx::query("INSERT INTO equity_snapshots(timestamp, cash, equity, position_quantity, price) VALUES (?, ?, ?, ?, ?)")
            .bind(timestamp).bind(state.cash).bind(equity).bind(state.position_quantity).bind(last_price)
            .execute(&self.pool).await?;
        sqlx::query("INSERT INTO bot_state(id, cash, position_quantity, entry_price, last_price, equity, updated_at) VALUES (1, ?, ?, ?, ?, ?, ?) ON CONFLICT(id) DO UPDATE SET cash=excluded.cash, position_quantity=excluded.position_quantity, entry_price=excluded.entry_price, last_price=excluded.last_price, equity=excluded.equity, updated_at=excluded.updated_at")
            .bind(state.cash).bind(state.position_quantity).bind(state.entry_price)
            .bind(last_price).bind(equity).bind(timestamp).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn load_trader_state(&self) -> Result<Option<TraderState>> {
        let row = sqlx::query_as::<_, StatusRow>("SELECT cash, position_quantity, entry_price, last_price, equity, updated_at FROM bot_state WHERE id=1")
            .fetch_optional(&self.pool).await?;
        Ok(row.map(|row| TraderState {
            cash: row.cash,
            position_quantity: row.position_quantity,
            entry_price: row.entry_price,
        }))
    }

    pub async fn status(&self) -> Result<Option<StatusRow>> {
        Ok(sqlx::query_as("SELECT cash, position_quantity, entry_price, last_price, equity, updated_at FROM bot_state WHERE id=1")
            .fetch_optional(&self.pool).await?)
    }

    pub async fn recent_trades(&self, limit: i64) -> Result<Vec<TradeRow>> {
        Ok(sqlx::query_as("SELECT id, timestamp, symbol, side, price, quantity, fee, realized_pnl, reason FROM trades ORDER BY timestamp DESC LIMIT ?")
            .bind(limit).fetch_all(&self.pool).await?)
    }

    pub async fn recent_equity(&self, limit: i64) -> Result<Vec<EquityRow>> {
        let mut rows: Vec<EquityRow> = sqlx::query_as("SELECT timestamp, cash, equity, position_quantity, price FROM equity_snapshots ORDER BY timestamp DESC LIMIT ?")
            .bind(limit).fetch_all(&self.pool).await?;
        rows.reverse();
        Ok(rows)
    }

    pub async fn recent_candles(
        &self,
        symbol: &str,
        interval: &str,
        limit: i64,
    ) -> Result<Vec<CandleRow>> {
        let mut rows: Vec<CandleRow> = sqlx::query_as("SELECT close_time, close FROM candles WHERE symbol=? AND interval=? ORDER BY close_time DESC LIMIT ?")
            .bind(symbol).bind(interval).bind(limit).fetch_all(&self.pool).await?;
        rows.reverse();
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exchange_trade_summary_is_idempotent_by_order_id() {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        let trade = Trade {
            timestamp: 1,
            side: "SELL",
            price: 100.0,
            quantity: 0.1,
            fee: 0.01,
            realized_pnl: 1.0,
            reason: "test",
        };
        assert!(
            database
                .record_exchange_trade_once("testnet", "BTCUSDT", 42, &trade)
                .await
                .unwrap()
        );
        assert!(
            !database
                .record_exchange_trade_once("testnet", "BTCUSDT", 42, &trade)
                .await
                .unwrap()
        );
        assert_eq!(database.recent_trades(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn risk_halt_persists_until_next_utc_day() {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        let day = 86_400_000;
        database
            .record_trade(
                "BTCUSDT",
                &Trade {
                    timestamp: day + 1,
                    side: "SELL",
                    price: 100.0,
                    quantity: 0.1,
                    fee: 0.0,
                    realized_pnl: -6.0,
                    reason: "test loss",
                },
            )
            .await
            .unwrap();
        let limits = RiskLimits {
            max_daily_loss_quote: 5.0,
            max_entries_per_day: 10,
            max_consecutive_losses: 3,
        };
        let halted = database
            .evaluate_risk("testnet", "BTCUSDT", day + 2, limits)
            .await
            .unwrap();
        assert!(halted.halted);
        let next_day = database
            .evaluate_risk("testnet", "BTCUSDT", day * 2, limits)
            .await
            .unwrap();
        assert!(!next_day.halted);
        assert_eq!(next_day.daily_realized_pnl, 0.0);
    }

    #[tokio::test]
    async fn risk_limits_cover_entries_and_consecutive_losses() {
        let entry_database = Database::connect("sqlite::memory:").await.unwrap();
        let limits = RiskLimits {
            max_daily_loss_quote: 100.0,
            max_entries_per_day: 2,
            max_consecutive_losses: 2,
        };
        for timestamp in [1, 2] {
            entry_database
                .record_trade(
                    "BTCUSDT",
                    &Trade {
                        timestamp,
                        side: "BUY",
                        price: 100.0,
                        quantity: 0.1,
                        fee: 0.0,
                        realized_pnl: 0.0,
                        reason: "test entry",
                    },
                )
                .await
                .unwrap();
        }
        let entry_halt = entry_database
            .evaluate_risk("testnet", "BTCUSDT", 3, limits)
            .await
            .unwrap();
        assert!(entry_halt.halted);
        assert_eq!(entry_halt.entries_today, 2);

        let loss_database = Database::connect("sqlite::memory:").await.unwrap();
        for timestamp in [1, 2] {
            loss_database
                .record_trade(
                    "BTCUSDT",
                    &Trade {
                        timestamp,
                        side: "SELL",
                        price: 100.0,
                        quantity: 0.1,
                        fee: 0.0,
                        realized_pnl: -1.0,
                        reason: "test loss",
                    },
                )
                .await
                .unwrap();
        }
        let loss_halt = loss_database
            .evaluate_risk("testnet", "BTCUSDT", 3, limits)
            .await
            .unwrap();
        assert!(loss_halt.halted);
        assert_eq!(loss_halt.consecutive_losses, 2);
    }
}
