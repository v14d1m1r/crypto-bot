use std::{env, str::FromStr};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    Paper,
    Testnet,
}

impl std::fmt::Display for ExecutionMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Paper => "paper",
            Self::Testnet => "testnet",
        })
    }
}

#[derive(Clone)]
pub struct Config {
    pub mode: ExecutionMode,
    pub symbol: String,
    pub interval: String,
    pub fast_ema: usize,
    pub slow_ema: usize,
    pub starting_cash: f64,
    pub position_fraction: f64,
    pub fee_rate: f64,
    pub stop_loss: f64,
    pub take_profit: f64,
    pub rest_base: String,
    pub ws_base: String,
    pub database_url: String,
    pub api_address: String,
    pub backtest_limit: usize,
    pub testnet_api_key: Option<String>,
    pub testnet_secret_key: Option<String>,
    pub max_order_quote: f64,
    pub max_daily_loss_quote: f64,
    pub max_entries_per_day: i64,
    pub max_consecutive_losses: i64,
    pub testnet_ws_api_base: String,
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<String>,
    pub telegram_api_base: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let mode = match env::var("BOT_MODE")
            .unwrap_or_else(|_| "paper".into())
            .as_str()
        {
            "paper" => ExecutionMode::Paper,
            "testnet" => ExecutionMode::Testnet,
            value => bail!("unsupported BOT_MODE '{value}'; use paper or testnet"),
        };
        let (default_rest, default_ws, default_database) = match mode {
            ExecutionMode::Paper => (
                "https://api.binance.com",
                "wss://stream.binance.com:9443/ws",
                "sqlite://crypto_bot.db?mode=rwc",
            ),
            ExecutionMode::Testnet => (
                "https://testnet.binance.vision",
                "wss://stream.testnet.binance.vision/ws",
                "sqlite://crypto_bot_testnet.db?mode=rwc",
            ),
        };
        let config = Self {
            mode,
            symbol: env::var("BOT_SYMBOL")
                .unwrap_or_else(|_| "BTCUSDT".into())
                .to_uppercase(),
            interval: env::var("BOT_INTERVAL").unwrap_or_else(|_| "1m".into()),
            fast_ema: env_value("BOT_FAST_EMA", 20)?,
            slow_ema: env_value("BOT_SLOW_EMA", 50)?,
            starting_cash: env_value("BOT_STARTING_CASH", 10_000.0)?,
            position_fraction: env_value("BOT_POSITION_FRACTION", 0.25)?,
            fee_rate: env_value("BOT_FEE_RATE", 0.001)?,
            stop_loss: env_value("BOT_STOP_LOSS", 0.02)?,
            take_profit: env_value("BOT_TAKE_PROFIT", 0.04)?,
            rest_base: endpoint_env(mode, "REST", default_rest),
            ws_base: endpoint_env(mode, "WS", default_ws),
            database_url: match mode {
                ExecutionMode::Paper => env::var("DATABASE_URL"),
                ExecutionMode::Testnet => env::var("TESTNET_DATABASE_URL"),
            }
            .unwrap_or_else(|_| default_database.into()),
            api_address: env::var("BOT_API_ADDRESS").unwrap_or_else(|_| "127.0.0.1:3001".into()),
            backtest_limit: env_value("BOT_BACKTEST_LIMIT", 500)?,
            testnet_api_key: optional_secret("BINANCE_TESTNET_API_KEY")?,
            testnet_secret_key: optional_secret("BINANCE_TESTNET_SECRET_KEY")?,
            max_order_quote: env_value("BOT_MAX_ORDER_QUOTE", 25.0)?,
            max_daily_loss_quote: env_value("BOT_MAX_DAILY_LOSS_QUOTE", 10.0)?,
            max_entries_per_day: env_value("BOT_MAX_ENTRIES_PER_DAY", 10)?,
            max_consecutive_losses: env_value("BOT_MAX_CONSECUTIVE_LOSSES", 3)?,
            testnet_ws_api_base: env::var("BINANCE_TESTNET_WS_API_BASE")
                .unwrap_or_else(|_| "wss://ws-api.testnet.binance.vision/ws-api/v3".into()),
            telegram_bot_token: optional_secret("TELEGRAM_BOT_TOKEN")?,
            telegram_chat_id: env::var("TELEGRAM_CHAT_ID")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            telegram_api_base: env::var("TELEGRAM_API_BASE")
                .unwrap_or_else(|_| "https://api.telegram.org".into()),
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.symbol.is_empty() || !self.symbol.chars().all(|c| c.is_ascii_alphanumeric()) {
            bail!("BOT_SYMBOL must contain only ASCII letters and digits");
        }
        const INTERVALS: &[&str] = &[
            "1s", "1m", "3m", "5m", "15m", "30m", "1h", "2h", "4h", "6h", "8h", "12h", "1d", "3d",
            "1w", "1M",
        ];
        if !INTERVALS.contains(&self.interval.as_str()) {
            bail!("unsupported BOT_INTERVAL: {}", self.interval);
        }
        if self.fast_ema == 0 || self.fast_ema >= self.slow_ema || self.slow_ema > 1_000 {
            bail!("EMA windows must satisfy 0 < BOT_FAST_EMA < BOT_SLOW_EMA <= 1000");
        }
        if self.starting_cash <= 0.0 {
            bail!("BOT_STARTING_CASH must be positive");
        }
        validate_fraction("BOT_POSITION_FRACTION", self.position_fraction, false)?;
        validate_fraction("BOT_FEE_RATE", self.fee_rate, true)?;
        validate_fraction("BOT_STOP_LOSS", self.stop_loss, false)?;
        validate_fraction("BOT_TAKE_PROFIT", self.take_profit, false)?;
        if !(self.slow_ema + 2..=1_000).contains(&self.backtest_limit) {
            bail!("BOT_BACKTEST_LIMIT must be larger than the slow EMA and at most 1000");
        }
        if !self.max_order_quote.is_finite() || self.max_order_quote <= 0.0 {
            bail!("BOT_MAX_ORDER_QUOTE must be positive");
        }
        if !self.max_daily_loss_quote.is_finite() || self.max_daily_loss_quote <= 0.0 {
            bail!("BOT_MAX_DAILY_LOSS_QUOTE must be positive");
        }
        if self.max_entries_per_day <= 0 {
            bail!("BOT_MAX_ENTRIES_PER_DAY must be positive");
        }
        if self.max_consecutive_losses <= 0 {
            bail!("BOT_MAX_CONSECUTIVE_LOSSES must be positive");
        }
        if self.telegram_bot_token.is_some() != self.telegram_chat_id.is_some() {
            bail!("TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID must be configured together");
        }
        if !self.telegram_api_base.starts_with("https://")
            && !self.telegram_api_base.starts_with("http://127.0.0.1:")
        {
            bail!("TELEGRAM_API_BASE must use HTTPS (or loopback HTTP for tests)");
        }
        if self.mode == ExecutionMode::Testnet {
            if !self.rest_base.starts_with("https://testnet.binance.vision")
                && !self
                    .rest_base
                    .starts_with("https://api1.testnet.binance.vision")
            {
                bail!("testnet mode refuses a non-Testnet REST endpoint");
            }
            if !self
                .ws_base
                .starts_with("wss://stream.testnet.binance.vision")
            {
                bail!("testnet mode refuses a non-Testnet market-stream endpoint");
            }
            if !self
                .testnet_ws_api_base
                .starts_with("wss://ws-api.testnet.binance.vision")
            {
                bail!("testnet mode refuses a non-Testnet user-data endpoint");
            }
        }
        Ok(())
    }

    pub fn testnet_credentials(&self) -> Result<(&str, &str)> {
        if self.mode != ExecutionMode::Testnet {
            bail!("Testnet credentials requested while BOT_MODE is not testnet");
        }
        let key = self
            .testnet_api_key
            .as_deref()
            .context("BINANCE_TESTNET_API_KEY is required in testnet mode")?;
        let secret = self
            .testnet_secret_key
            .as_deref()
            .context("BINANCE_TESTNET_SECRET_KEY is required in testnet mode")?;
        Ok((key, secret))
    }

    #[cfg(test)]
    pub fn default_for_test() -> Self {
        Self {
            mode: ExecutionMode::Paper,
            symbol: "BTCUSDT".into(),
            interval: "1m".into(),
            fast_ema: 20,
            slow_ema: 50,
            starting_cash: 10_000.0,
            position_fraction: 0.25,
            fee_rate: 0.001,
            stop_loss: 0.02,
            take_profit: 0.04,
            rest_base: "https://api.binance.com".into(),
            ws_base: "wss://stream.binance.com:9443/ws".into(),
            database_url: "sqlite::memory:".into(),
            api_address: "127.0.0.1:3001".into(),
            backtest_limit: 500,
            testnet_api_key: None,
            testnet_secret_key: None,
            max_order_quote: 25.0,
            max_daily_loss_quote: 10.0,
            max_entries_per_day: 10,
            max_consecutive_losses: 3,
            testnet_ws_api_base: "wss://ws-api.testnet.binance.vision/ws-api/v3".into(),
            telegram_bot_token: None,
            telegram_chat_id: None,
            telegram_api_base: "https://api.telegram.org".into(),
        }
    }
}

fn endpoint_env(mode: ExecutionMode, kind: &str, default: &str) -> String {
    let name = match mode {
        ExecutionMode::Paper => format!("BINANCE_{kind}_BASE"),
        ExecutionMode::Testnet => format!("BINANCE_TESTNET_{kind}_BASE"),
    };
    env::var(name).unwrap_or_else(|_| default.into())
}

fn optional_secret(name: &str) -> Result<Option<String>> {
    match env::var(name) {
        Ok(value) if value.trim().is_empty() => Ok(None),
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("could not read {name}")),
    }
}

fn env_value<T>(name: &str, default: T) -> Result<T>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    match env::var(name) {
        Ok(value) => value.parse().with_context(|| format!("invalid {name}")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error).with_context(|| format!("could not read {name}")),
    }
}

fn validate_fraction(name: &str, value: f64, allow_zero: bool) -> Result<()> {
    let lower_ok = if allow_zero {
        value >= 0.0
    } else {
        value > 0.0
    };
    if !value.is_finite() || !lower_ok || value >= 1.0 {
        bail!(
            "{name} must be {} and less than 1",
            if allow_zero {
                "non-negative"
            } else {
                "positive"
            }
        );
    }
    Ok(())
}
