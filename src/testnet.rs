use std::{
    collections::HashMap,
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use reqwest::{Client, Method};
use rust_decimal::{Decimal, RoundingStrategy, prelude::ToPrimitive};
use serde::Deserialize;
use sha2::Sha256;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};
use url::form_urlencoded::Serializer;

use crate::{
    alerts::TelegramAlerter,
    binance::{self, Candle, StreamEnd},
    config::Config,
    database::{Database, RiskLimits, RiskStatusRow},
    strategy::{EmaCrossover, Signal},
    trader::{Trade, TraderState},
};

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerTime {
    server_time: i64,
}

#[derive(Debug, Deserialize)]
struct AveragePrice {
    price: String,
}

#[derive(Debug, Deserialize)]
struct ExchangeInfo {
    symbols: Vec<SymbolInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SymbolInfo {
    symbol: String,
    status: String,
    base_asset: String,
    quote_asset: String,
    quote_asset_precision: u32,
    filters: Vec<RawFilter>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawFilter {
    filter_type: String,
    min_qty: Option<String>,
    max_qty: Option<String>,
    step_size: Option<String>,
    min_notional: Option<String>,
    tick_size: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AccountResponse {
    balances: Vec<Balance>,
}

#[derive(Debug, Deserialize)]
struct Balance {
    asset: String,
    free: String,
    locked: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrderResponse {
    symbol: String,
    order_id: i64,
    client_order_id: String,
    #[serde(default = "negative_one")]
    order_list_id: i64,
    transact_time: Option<i64>,
    status: String,
    side: String,
    executed_qty: String,
    cummulative_quote_qty: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccountTrade {
    id: i64,
    order_id: i64,
    price: String,
    qty: String,
    quote_qty: String,
    commission: String,
    commission_asset: String,
    time: i64,
    is_buyer: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrderListResponse {
    order_list_id: i64,
    transaction_time: i64,
    list_order_status: String,
    orders: Vec<OrderSummary>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrderSummary {
    order_id: i64,
    client_order_id: String,
}

#[derive(Debug, Deserialize)]
struct UserEnvelope {
    event: Option<UserEvent>,
}

#[derive(Debug, Deserialize)]
struct UserEvent {
    #[serde(rename = "e")]
    event_type: String,
    #[serde(rename = "s", default)]
    symbol: String,
    #[serde(rename = "c", default)]
    client_order_id: String,
    #[serde(rename = "S", default)]
    side: String,
    #[serde(rename = "X", default)]
    status: String,
    #[serde(rename = "i", default)]
    order_id: i64,
    #[serde(rename = "z", default)]
    cumulative_quantity: String,
    #[serde(rename = "Z", default)]
    cumulative_quote: String,
    #[serde(rename = "T", default)]
    transaction_time: i64,
}

#[derive(Debug, Deserialize)]
struct BinanceError {
    code: i64,
    msg: String,
}

#[derive(Clone)]
pub struct TestnetClient {
    http: Client,
    api_key: String,
    secret_key: String,
    rest_base: String,
    time_offset_ms: i64,
}

#[derive(Debug, Clone)]
pub struct SymbolRules {
    pub base_asset: String,
    pub quote_asset: String,
    quote_precision: u32,
    min_quantity: Decimal,
    max_quantity: Decimal,
    step_size: Decimal,
    min_notional: Decimal,
    tick_size: Decimal,
}

#[derive(Debug)]
pub struct AccountSnapshot {
    balances: HashMap<String, (Decimal, Decimal)>,
}

#[derive(Debug)]
struct ExecutedOrder {
    order_id: i64,
    client_order_id: String,
    timestamp: i64,
    side: String,
    status: String,
    executed_base: Decimal,
    net_base: Decimal,
    gross_quote: Decimal,
    net_quote: Decimal,
    quote_fee_equivalent: Decimal,
    fills: Vec<AccountTrade>,
}

impl AccountSnapshot {
    fn free(&self, asset: &str) -> Decimal {
        self.balances
            .get(asset)
            .map(|balance| balance.0)
            .unwrap_or(Decimal::ZERO)
    }
    fn total(&self, asset: &str) -> Decimal {
        self.balances
            .get(asset)
            .map(|balance| balance.0 + balance.1)
            .unwrap_or(Decimal::ZERO)
    }
}

impl TestnetClient {
    pub async fn connect(config: &Config) -> Result<Self> {
        let (api_key, secret_key) = config.testnet_credentials()?;
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?;
        let rest_base = config.rest_base.trim_end_matches('/').to_owned();
        let server: ServerTime = http
            .get(format!("{rest_base}/api/v3/time"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("invalid Binance Testnet server-time response")?;
        let local_time = now_ms()?;
        Ok(Self {
            http,
            api_key: api_key.into(),
            secret_key: secret_key.into(),
            rest_base,
            time_offset_ms: server.server_time - local_time,
        })
    }

    pub async fn symbol_rules(&self, symbol: &str) -> Result<SymbolRules> {
        let response: ExchangeInfo = self
            .http
            .get(format!("{}/api/v3/exchangeInfo", self.rest_base))
            .query(&[("symbol", symbol)])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let info = response
            .symbols
            .into_iter()
            .next()
            .context("symbol missing from Testnet exchangeInfo")?;
        if info.symbol != symbol || info.status != "TRADING" {
            bail!("{symbol} is not available for Testnet trading");
        }
        let market = info
            .filters
            .iter()
            .find(|filter| filter.filter_type == "MARKET_LOT_SIZE");
        let lot = info
            .filters
            .iter()
            .find(|filter| filter.filter_type == "LOT_SIZE");
        let selected = market
            .filter(|filter| decimal_or_zero(filter.step_size.as_deref()) > Decimal::ZERO)
            .or(lot)
            .context("symbol has no usable market quantity filter")?;
        let min_notional = info
            .filters
            .iter()
            .find(|filter| filter.filter_type == "NOTIONAL" || filter.filter_type == "MIN_NOTIONAL")
            .and_then(|filter| filter.min_notional.as_deref())
            .map(Decimal::from_str)
            .transpose()?
            .unwrap_or(Decimal::ZERO);
        let tick_size = info
            .filters
            .iter()
            .find(|filter| filter.filter_type == "PRICE_FILTER")
            .and_then(|filter| filter.tick_size.as_deref())
            .map(Decimal::from_str)
            .transpose()?
            .context("symbol has no PRICE_FILTER tick size")?;
        Ok(SymbolRules {
            base_asset: info.base_asset,
            quote_asset: info.quote_asset,
            quote_precision: info.quote_asset_precision,
            min_quantity: parse_required(&selected.min_qty, "minQty")?,
            max_quantity: parse_required(&selected.max_qty, "maxQty")?,
            step_size: parse_required(&selected.step_size, "stepSize")?,
            min_notional,
            tick_size,
        })
    }

    pub async fn account(&self) -> Result<AccountSnapshot> {
        let response: AccountResponse = self
            .signed_json(
                Method::GET,
                "/api/v3/account",
                vec![("omitZeroBalances", "true".into())],
            )
            .await?;
        let balances = response
            .balances
            .into_iter()
            .map(|balance| {
                Ok((
                    balance.asset,
                    (
                        Decimal::from_str(&balance.free)?,
                        Decimal::from_str(&balance.locked)?,
                    ),
                ))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        Ok(AccountSnapshot { balances })
    }

    async fn current_price(&self, symbol: &str) -> Result<Decimal> {
        let response: AveragePrice = self
            .http
            .get(format!("{}/api/v3/avgPrice", self.rest_base))
            .query(&[("symbol", symbol)])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(Decimal::from_str(&response.price)?)
    }

    pub async fn validate_market_buy(&self, config: &Config, rules: &SymbolRules) -> Result<()> {
        let quote = rules.round_quote(
            Decimal::from_f64_retain(config.max_order_quote)
                .context("invalid BOT_MAX_ORDER_QUOTE")?,
        );
        rules.validate_notional(quote)?;
        let _: serde_json::Value = self
            .signed_json(
                Method::POST,
                "/api/v3/order/test",
                vec![
                    ("symbol", config.symbol.clone()),
                    ("side", "BUY".into()),
                    ("type", "MARKET".into()),
                    ("quoteOrderQty", quote.normalize().to_string()),
                ],
            )
            .await?;
        Ok(())
    }

    async fn market_buy(
        &self,
        symbol: &str,
        quote: Decimal,
        rules: &SymbolRules,
    ) -> Result<ExecutedOrder> {
        rules.validate_notional(quote)?;
        self.place_market_order(
            symbol,
            "BUY",
            "quoteOrderQty",
            rules.round_quote(quote).normalize().to_string(),
            rules,
        )
        .await
    }

    async fn market_sell(
        &self,
        symbol: &str,
        quantity: Decimal,
        rules: &SymbolRules,
    ) -> Result<ExecutedOrder> {
        let quantity = rules.round_quantity(quantity);
        rules.validate_quantity(quantity)?;
        self.place_market_order(
            symbol,
            "SELL",
            "quantity",
            quantity.normalize().to_string(),
            rules,
        )
        .await
    }

    async fn place_market_order(
        &self,
        symbol: &str,
        side: &str,
        quantity_name: &str,
        quantity: String,
        rules: &SymbolRules,
    ) -> Result<ExecutedOrder> {
        let client_id = format!(
            "crux{}{}",
            side.chars().next().unwrap_or('X'),
            self.timestamp()?
        );
        let params = vec![
            ("symbol", symbol.into()),
            ("side", side.into()),
            ("type", "MARKET".into()),
            (quantity_name, quantity),
            ("newClientOrderId", client_id.clone()),
            ("newOrderRespType", "FULL".into()),
        ];
        let response = match self
            .signed_response(Method::POST, "/api/v3/order", params)
            .await
        {
            Ok(response) if response.status().is_success() => {
                response.json::<OrderResponse>().await?
            }
            Ok(response) if response.status().is_server_error() => {
                warn!(status = %response.status(), %client_id, "order response uncertain; querying by client order ID");
                self.query_order(symbol, &client_id).await?
            }
            Ok(response) => return Err(binance_error(response).await),
            Err(error) => {
                warn!(%error, %client_id, "order request failed; querying before any retry");
                self.query_order(symbol, &client_id)
                    .await
                    .context("order status remained unknown after request failure")?
            }
        };
        let trades = self
            .account_trades_for_order(symbol, response.order_id)
            .await?;
        ExecutedOrder::from_response_and_trades(response, trades, rules)
    }

    async fn query_order(&self, symbol: &str, client_id: &str) -> Result<OrderResponse> {
        self.signed_json(
            Method::GET,
            "/api/v3/order",
            vec![
                ("symbol", symbol.into()),
                ("origClientOrderId", client_id.into()),
            ],
        )
        .await
    }

    async fn query_order_by_id(&self, symbol: &str, order_id: i64) -> Result<OrderResponse> {
        self.signed_json(
            Method::GET,
            "/api/v3/order",
            vec![("symbol", symbol.into()), ("orderId", order_id.to_string())],
        )
        .await
    }

    async fn open_orders(&self, symbol: &str) -> Result<Vec<OrderResponse>> {
        self.signed_json(
            Method::GET,
            "/api/v3/openOrders",
            vec![("symbol", symbol.into())],
        )
        .await
    }

    async fn recent_orders(&self, symbol: &str) -> Result<Vec<OrderResponse>> {
        self.signed_json(
            Method::GET,
            "/api/v3/allOrders",
            vec![("symbol", symbol.into()), ("limit", "100".into())],
        )
        .await
    }

    async fn recent_account_trades(&self, symbol: &str) -> Result<Vec<AccountTrade>> {
        self.signed_json(
            Method::GET,
            "/api/v3/myTrades",
            vec![("symbol", symbol.into()), ("limit", "1000".into())],
        )
        .await
    }

    async fn account_trades_for_order(
        &self,
        symbol: &str,
        order_id: i64,
    ) -> Result<Vec<AccountTrade>> {
        self.signed_json(
            Method::GET,
            "/api/v3/myTrades",
            vec![
                ("symbol", symbol.into()),
                ("orderId", order_id.to_string()),
                ("limit", "1000".into()),
            ],
        )
        .await
    }

    async fn place_protection(
        &self,
        symbol: &str,
        quantity: Decimal,
        entry: Decimal,
        stop_loss: f64,
        take_profit: f64,
        rules: &SymbolRules,
    ) -> Result<OrderListResponse> {
        let quantity = rules.round_quantity(quantity);
        rules.validate_quantity(quantity)?;
        let stop = rules.price_down(
            entry
                * (Decimal::ONE
                    - Decimal::from_f64_retain(stop_loss).context("invalid stop loss")?),
        );
        let take = rules.price_up(
            entry
                * (Decimal::ONE
                    + Decimal::from_f64_retain(take_profit).context("invalid take profit")?),
        );
        let stamp = self.timestamp()?;
        let list_id = format!("cruxO{stamp}");
        self.signed_json(
            Method::POST,
            "/api/v3/orderList/oco",
            vec![
                ("symbol", symbol.into()),
                ("side", "SELL".into()),
                ("quantity", quantity.normalize().to_string()),
                ("listClientOrderId", list_id),
                ("aboveType", "TAKE_PROFIT".into()),
                ("aboveStopPrice", take.normalize().to_string()),
                ("aboveClientOrderId", format!("cruxT{stamp}")),
                ("belowType", "STOP_LOSS".into()),
                ("belowStopPrice", stop.normalize().to_string()),
                ("belowClientOrderId", format!("cruxS{stamp}")),
                ("newOrderRespType", "RESULT".into()),
            ],
        )
        .await
    }

    async fn cancel_protection(&self, symbol: &str, order_list_id: i64) -> Result<()> {
        let _: serde_json::Value = self
            .signed_json(
                Method::DELETE,
                "/api/v3/orderList",
                vec![
                    ("symbol", symbol.into()),
                    ("orderListId", order_list_id.to_string()),
                ],
            )
            .await?;
        Ok(())
    }

    fn user_subscription(&self) -> Result<String> {
        let timestamp = self.timestamp()?;
        let query = format!(
            "apiKey={}&recvWindow=5000&timestamp={timestamp}",
            self.api_key
        );
        let signature = sign_query(&self.secret_key, &query)?;
        Ok(serde_json::json!({
            "id": format!("crux-sub-{timestamp}"),
            "method": "userDataStream.subscribe.signature",
            "params": { "apiKey": self.api_key, "recvWindow": 5000, "timestamp": timestamp, "signature": signature }
        }).to_string())
    }

    async fn signed_json<T: serde::de::DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        params: Vec<(&str, String)>,
    ) -> Result<T> {
        let response = self.signed_response(method, path, params).await?;
        if !response.status().is_success() {
            return Err(binance_error(response).await);
        }
        response
            .json()
            .await
            .context("invalid signed Binance Testnet response")
    }

    async fn signed_response(
        &self,
        method: Method,
        path: &str,
        mut params: Vec<(&str, String)>,
    ) -> Result<reqwest::Response> {
        params.push(("recvWindow", "5000".into()));
        params.push(("timestamp", self.timestamp()?.to_string()));
        let mut serializer = Serializer::new(String::new());
        for (key, value) in &params {
            serializer.append_pair(key, value);
        }
        let query = serializer.finish();
        let signature = sign_query(&self.secret_key, &query)?;
        let url = format!(
            "{}{}?{}&signature={}",
            self.rest_base, path, query, signature
        );
        Ok(self
            .http
            .request(method, url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await?)
    }

    fn timestamp(&self) -> Result<i64> {
        Ok(now_ms()? + self.time_offset_ms)
    }
}

impl SymbolRules {
    fn round_quantity(&self, quantity: Decimal) -> Decimal {
        if self.step_size.is_zero() {
            return quantity;
        }
        (quantity / self.step_size).floor() * self.step_size
    }
    fn round_quote(&self, quote: Decimal) -> Decimal {
        quote.round_dp_with_strategy(self.quote_precision, RoundingStrategy::ToZero)
    }
    fn validate_quantity(&self, quantity: Decimal) -> Result<()> {
        if quantity < self.min_quantity || quantity > self.max_quantity {
            bail!(
                "quantity {quantity} violates Testnet market limits {}..={}",
                self.min_quantity,
                self.max_quantity
            );
        }
        Ok(())
    }
    fn validate_notional(&self, quote: Decimal) -> Result<()> {
        if quote < self.min_notional {
            bail!(
                "order value {quote} {} is below Testnet minimum {}",
                self.quote_asset,
                self.min_notional
            );
        }
        Ok(())
    }
    fn price_down(&self, price: Decimal) -> Decimal {
        (price / self.tick_size).floor() * self.tick_size
    }
    fn price_up(&self, price: Decimal) -> Decimal {
        (price / self.tick_size).ceil() * self.tick_size
    }
}

impl ExecutedOrder {
    fn from_response_and_trades(
        response: OrderResponse,
        trades: Vec<AccountTrade>,
        rules: &SymbolRules,
    ) -> Result<Self> {
        if response.symbol.is_empty() || trades.is_empty() {
            bail!("Testnet order {} has no account trades", response.order_id);
        }
        let is_buy = response.side == "BUY";
        let mut executed_base = Decimal::ZERO;
        let mut gross_quote = Decimal::ZERO;
        let mut base_commission = Decimal::ZERO;
        let mut quote_commission = Decimal::ZERO;
        let mut quote_fee_equivalent = Decimal::ZERO;
        let mut timestamp = response.transact_time.unwrap_or(now_ms()?);
        for trade in &trades {
            if trade.order_id != response.order_id || trade.is_buyer != is_buy {
                bail!(
                    "account trade does not match Testnet order {}",
                    response.order_id
                );
            }
            let price = Decimal::from_str(&trade.price)?;
            let quantity = Decimal::from_str(&trade.qty)?;
            let quote_quantity = Decimal::from_str(&trade.quote_qty)?;
            let commission = Decimal::from_str(&trade.commission)?;
            executed_base += quantity;
            gross_quote += quote_quantity;
            timestamp = timestamp.max(trade.time);
            if trade.commission_asset == rules.base_asset {
                base_commission += commission;
                quote_fee_equivalent += commission * price;
            } else if trade.commission_asset == rules.quote_asset {
                quote_commission += commission;
                quote_fee_equivalent += commission;
            }
        }
        if executed_base.is_zero() {
            bail!(
                "Testnet order {} has zero executed quantity",
                response.order_id
            );
        }
        let net_base = if is_buy {
            executed_base - base_commission
        } else {
            executed_base + base_commission
        };
        let net_quote = if is_buy {
            gross_quote + quote_commission
        } else {
            gross_quote - quote_commission
        };
        Ok(Self {
            order_id: response.order_id,
            client_order_id: response.client_order_id,
            timestamp,
            side: response.side,
            status: response.status,
            executed_base,
            net_base,
            gross_quote,
            net_quote,
            quote_fee_equivalent,
            fills: trades,
        })
    }
}

pub struct TestnetTrader {
    client: TestnetClient,
    rules: SymbolRules,
    tracked_quantity: Decimal,
    entry_price: Option<Decimal>,
    protective_list_id: Option<i64>,
    alerts: TelegramAlerter,
    last_risk_alert: Option<(i64, String)>,
    last_reconnect_alert_ms: Option<i64>,
    connection_warning_active: bool,
}

impl TestnetTrader {
    fn risk_limits(config: &Config) -> RiskLimits {
        RiskLimits {
            max_daily_loss_quote: config.max_daily_loss_quote,
            max_entries_per_day: config.max_entries_per_day,
            max_consecutive_losses: config.max_consecutive_losses,
        }
    }

    async fn refresh_risk(
        &mut self,
        config: &Config,
        database: &Database,
        timestamp: i64,
    ) -> Result<RiskStatusRow> {
        let risk = database
            .evaluate_risk(
                "testnet",
                &config.symbol,
                timestamp,
                Self::risk_limits(config),
            )
            .await?;
        if risk.halted {
            let reason = risk.reason.as_deref().unwrap_or("risk limit reached");
            warn!(
                reason,
                daily_realized_pnl = risk.daily_realized_pnl,
                entries_today = risk.entries_today,
                consecutive_losses = risk.consecutive_losses,
                "Testnet risk circuit breaker is active; new buys are disabled"
            );
            let key = (risk.utc_day_start, reason.to_owned());
            if self.last_risk_alert.as_ref() != Some(&key) {
                self.alerts.notify(format!(
                    "CRUX TESTNET RISK HALT\n{}\nDaily PnL: {:.8}\nEntries: {}\nConsecutive losses: {}",
                    reason,
                    risk.daily_realized_pnl,
                    risk.entries_today,
                    risk.consecutive_losses
                ));
                self.last_risk_alert = Some(key);
            }
        } else {
            self.last_risk_alert = None;
        }
        Ok(risk)
    }

    pub async fn initialize(config: &Config, database: &Database) -> Result<Self> {
        let client = TestnetClient::connect(config).await?;
        let rules = client.symbol_rules(&config.symbol).await?;
        let saved = database.load_trader_state().await?;
        let saved_quantity = saved
            .and_then(|state| state.position_quantity)
            .and_then(Decimal::from_f64_retain)
            .unwrap_or(Decimal::ZERO);
        let entry_price = if saved_quantity > Decimal::ZERO {
            saved
                .and_then(|state| state.entry_price)
                .and_then(Decimal::from_f64_retain)
        } else {
            None
        };
        let mut trader = Self {
            client,
            rules,
            tracked_quantity: saved_quantity,
            entry_price,
            protective_list_id: None,
            alerts: TelegramAlerter::from_config(config),
            last_risk_alert: None,
            last_reconnect_alert_ms: None,
            connection_warning_active: false,
        };
        trader.reconcile(config, database).await?;
        Ok(trader)
    }

    /// Refresh local execution state from Binance before processing more signals.
    ///
    /// This is deliberately called after every user-data subscription, not only
    /// during process startup, because order events can be missed while either
    /// WebSocket is disconnected.
    async fn reconcile(&mut self, config: &Config, database: &Database) -> Result<()> {
        let account = self
            .client
            .account()
            .await
            .context("Testnet credentials or account permissions are invalid")?;
        let recent_orders = self.client.recent_orders(&config.symbol).await?;
        let bot_order_ids = recent_orders
            .iter()
            .filter(|order| order.client_order_id.starts_with("crux"))
            .map(|order| order.order_id)
            .collect::<std::collections::HashSet<_>>();
        let account_trades = self.client.recent_account_trades(&config.symbol).await?;
        for fill in account_trades
            .iter()
            .filter(|fill| bot_order_ids.contains(&fill.order_id))
        {
            Self::record_fill(database, &config.symbol, fill).await?;
        }
        if self.tracked_quantity.is_zero() {
            let (recovered_quantity, recovered_entry) =
                self.reconstruct_open_position(&account_trades, &bot_order_ids)?;
            self.tracked_quantity = recovered_quantity.min(account.total(&self.rules.base_asset));
            self.entry_price = if self.tracked_quantity >= self.rules.min_quantity {
                recovered_entry
            } else {
                self.tracked_quantity = Decimal::ZERO;
                None
            };
            if self.tracked_quantity > Decimal::ZERO {
                warn!(quantity = %self.tracked_quantity, entry_price = ?self.entry_price,
                    "recovered Testnet position from exact account trades after missing local state");
            }
        }
        let mut missed_sells = Vec::new();
        for order in &recent_orders {
            if order.client_order_id.starts_with("crux") {
                let previous = database
                    .exchange_order_status("testnet", order.order_id)
                    .await?;
                if order.side == "SELL"
                    && order.status == "FILLED"
                    && previous.as_deref() != Some("FILLED")
                {
                    let fills = account_trades
                        .iter()
                        .filter(|fill| fill.order_id == order.order_id)
                        .cloned()
                        .collect::<Vec<_>>();
                    if !fills.is_empty() {
                        missed_sells.push((order.clone(), fills));
                    }
                }
            }
        }
        missed_sells.sort_by_key(|(order, _)| std::cmp::Reverse(order.order_id));
        if self.tracked_quantity > Decimal::ZERO
            && let Some((order, fills)) = missed_sells.into_iter().next()
        {
            let executed = ExecutedOrder::from_response_and_trades(order, fills, &self.rules)?;
            let reason = if executed.client_order_id.starts_with("cruxT") {
                "recovered exchange take profit"
            } else if executed.client_order_id.starts_with("cruxS") {
                "recovered exchange stop/strategy sell"
            } else {
                "recovered exchange execution"
            };
            self.apply_reconciled_sell(config, database, executed, reason)
                .await?;
        }
        for order in &recent_orders {
            if order.client_order_id.starts_with("crux") {
                database
                    .record_exchange_order(
                        "testnet",
                        &config.symbol,
                        order.order_id,
                        &order.client_order_id,
                        &order.side,
                        &order.status,
                        decimal_f64(Decimal::from_str(&order.executed_qty)?)?,
                        decimal_f64(Decimal::from_str(&order.cummulative_quote_qty)?)?,
                        now_ms()?,
                    )
                    .await?;
            }
        }
        self.protective_list_id = self
            .client
            .open_orders(&config.symbol)
            .await?
            .into_iter()
            .find(|order| order.client_order_id.starts_with("crux") && order.order_list_id >= 0)
            .map(|order| order.order_list_id);

        let previous_quantity = self.tracked_quantity;
        self.tracked_quantity = self
            .tracked_quantity
            .min(account.total(&self.rules.base_asset));
        if self.tracked_quantity < self.rules.min_quantity {
            self.tracked_quantity = Decimal::ZERO;
            self.entry_price = None;
            self.protective_list_id = None;
        }
        if previous_quantity != self.tracked_quantity {
            warn!(previous = %previous_quantity, reconciled = %self.tracked_quantity,
                "local Testnet position adjusted to Binance balance");
        }
        if self.tracked_quantity > Decimal::ZERO && self.protective_list_id.is_none() {
            self.install_protection(config, database)
                .await
                .context("tracked Testnet position has no exchange-hosted protection")?;
        }

        let price = self.client.current_price(&config.symbol).await?;
        let cash = account.free(&self.rules.quote_asset);
        let equity = cash + self.tracked_quantity * price;
        let timestamp = now_ms()?;
        database
            .save_state(
                timestamp,
                TraderState {
                    cash: decimal_f64(cash)?,
                    position_quantity: if self.tracked_quantity.is_zero() {
                        None
                    } else {
                        Some(decimal_f64(self.tracked_quantity)?)
                    },
                    entry_price: self.entry_price.map(decimal_f64).transpose()?,
                },
                decimal_f64(price)?,
                decimal_f64(equity)?,
            )
            .await?;
        self.refresh_risk(config, database, timestamp).await?;
        info!(base = %self.rules.base_asset, quote = %self.rules.quote_asset,
            quote_free = %cash, base_total = %account.total(&self.rules.base_asset),
            tracked_quantity = %self.tracked_quantity, protective_list_id = ?self.protective_list_id,
            "Binance Spot Testnet state reconciled");
        Ok(())
    }

    pub async fn send_alert(&self, text: &str) {
        if self.alerts.enabled()
            && let Err(error) = self.alerts.send(text).await
        {
            warn!(%error, "Telegram lifecycle alert delivery failed");
        }
    }

    pub async fn flush_alerts(&self) {
        self.alerts.flush().await;
    }

    pub fn notify_reconnect(&mut self, text: impl Into<String>) {
        self.connection_warning_active = true;
        let timestamp = now_ms().unwrap_or_default();
        if self
            .last_reconnect_alert_ms
            .is_none_or(|last| timestamp - last >= 300_000)
        {
            self.alerts.notify(text);
            self.last_reconnect_alert_ms = Some(timestamp);
        }
    }

    pub fn notify_recovered(&mut self) {
        if self.connection_warning_active {
            self.alerts.notify(
                "CRUX TESTNET CONNECTION RECOVERED\nMarket and user-data streams reconciled.",
            );
            self.connection_warning_active = false;
        }
    }

    async fn record_fill(database: &Database, symbol: &str, fill: &AccountTrade) -> Result<()> {
        database
            .record_exchange_fill(
                "testnet",
                symbol,
                fill.id,
                fill.order_id,
                decimal_f64(Decimal::from_str(&fill.price)?)?,
                decimal_f64(Decimal::from_str(&fill.qty)?)?,
                decimal_f64(Decimal::from_str(&fill.quote_qty)?)?,
                decimal_f64(Decimal::from_str(&fill.commission)?)?,
                &fill.commission_asset,
                fill.time,
            )
            .await
    }

    fn reconstruct_open_position(
        &self,
        account_trades: &[AccountTrade],
        bot_order_ids: &std::collections::HashSet<i64>,
    ) -> Result<(Decimal, Option<Decimal>)> {
        let mut fills = account_trades
            .iter()
            .filter(|fill| bot_order_ids.contains(&fill.order_id))
            .collect::<Vec<_>>();
        fills.sort_by_key(|fill| (fill.time, fill.id));
        let mut quantity = Decimal::ZERO;
        let mut cost = Decimal::ZERO;
        for fill in fills {
            let fill_quantity = Decimal::from_str(&fill.qty)?;
            let quote = Decimal::from_str(&fill.quote_qty)?;
            let commission = Decimal::from_str(&fill.commission)?;
            let base_commission = if fill.commission_asset == self.rules.base_asset {
                commission
            } else {
                Decimal::ZERO
            };
            let quote_commission = if fill.commission_asset == self.rules.quote_asset {
                commission
            } else {
                Decimal::ZERO
            };
            if fill.is_buyer {
                quantity += fill_quantity - base_commission;
                cost += quote + quote_commission;
            } else if quantity > Decimal::ZERO {
                let debit = (fill_quantity + base_commission).min(quantity);
                cost -= cost / quantity * debit;
                quantity -= debit;
                if quantity < self.rules.min_quantity {
                    quantity = Decimal::ZERO;
                    cost = Decimal::ZERO;
                }
            }
        }
        let entry = if quantity > Decimal::ZERO {
            Some(cost / quantity)
        } else {
            None
        };
        Ok((quantity, entry))
    }

    async fn record_order_fills(
        database: &Database,
        symbol: &str,
        order: &ExecutedOrder,
    ) -> Result<()> {
        for fill in &order.fills {
            Self::record_fill(database, symbol, fill).await?;
        }
        Ok(())
    }

    async fn apply_reconciled_sell(
        &mut self,
        config: &Config,
        database: &Database,
        order: ExecutedOrder,
        reason: &'static str,
    ) -> Result<()> {
        let price = order.gross_quote / order.executed_base;
        let pnl = order.net_quote - self.entry_price.unwrap_or(price) * order.net_base;
        self.tracked_quantity = (self.tracked_quantity - order.net_base).max(Decimal::ZERO);
        if self.tracked_quantity < self.rules.min_quantity {
            self.tracked_quantity = Decimal::ZERO;
            self.entry_price = None;
            self.protective_list_id = None;
        }
        let inserted = database
            .record_exchange_trade_once(
                "testnet",
                &config.symbol,
                order.order_id,
                &Trade {
                    timestamp: order.timestamp,
                    side: "SELL",
                    price: decimal_f64(price)?,
                    quantity: decimal_f64(order.executed_base)?,
                    fee: decimal_f64(order.quote_fee_equivalent)?,
                    realized_pnl: decimal_f64(pnl)?,
                    reason,
                },
            )
            .await?;
        if inserted {
            self.alerts.notify(format!(
                "CRUX TESTNET SELL\n{}\nOrder: {}\nPrice: {:.8}\nQuantity: {:.8}\nFee: {:.8}\nRealized PnL: {:.8}",
                reason,
                order.order_id,
                decimal_f64(price)?,
                decimal_f64(order.executed_base)?,
                decimal_f64(order.quote_fee_equivalent)?,
                decimal_f64(pnl)?
            ));
        }
        info!(order_id = order.order_id, %reason, "Testnet sell reconciled from exact account trades");
        Ok(())
    }

    pub async fn validate(config: &Config) -> Result<()> {
        let client = TestnetClient::connect(config).await?;
        let rules = client.symbol_rules(&config.symbol).await?;
        let account = client.account().await?;
        client.validate_market_buy(config, &rules).await?;
        info!(symbol = %config.symbol, quote_free = %account.free(&rules.quote_asset),
            base_free = %account.free(&rules.base_asset), "credentials, account, filters, and test order validated");
        Ok(())
    }

    pub async fn manual(config: &Config, side: Signal) -> Result<()> {
        if std::env::var("BOT_MANUAL_CONFIRM").as_deref() != Ok("TESTNET_ONLY") {
            bail!("manual Testnet orders require BOT_MANUAL_CONFIRM=TESTNET_ONLY");
        }
        let database = Database::connect(&config.database_url).await?;
        let mut trader = Self::initialize(config, &database).await?;
        if side == Signal::Buy && !trader.tracked_quantity.is_zero() {
            bail!("manual buy refused because a tracked position already exists");
        }
        if side == Signal::Sell && trader.tracked_quantity.is_zero() {
            bail!("manual sell refused because there is no tracked position");
        }
        let price = trader.client.current_price(&config.symbol).await?;
        let candle = Candle {
            close_time: now_ms()?,
            close: decimal_f64(price)?,
        };
        let result = trader
            .on_candle(config, &database, &candle, Some(side))
            .await;
        trader.flush_alerts().await;
        result
    }

    async fn install_protection(&mut self, config: &Config, database: &Database) -> Result<()> {
        let entry = self
            .entry_price
            .context("cannot protect a position without entry price")?;
        let list = self
            .client
            .place_protection(
                &config.symbol,
                self.tracked_quantity,
                entry,
                config.stop_loss,
                config.take_profit,
                &self.rules,
            )
            .await?;
        for order in &list.orders {
            database
                .record_exchange_order(
                    "testnet",
                    &config.symbol,
                    order.order_id,
                    &order.client_order_id,
                    "SELL",
                    &list.list_order_status,
                    0.0,
                    0.0,
                    list.transaction_time,
                )
                .await?;
        }
        self.protective_list_id = Some(list.order_list_id);
        info!(
            order_list_id = list.order_list_id,
            "exchange-hosted Testnet stop-loss/take-profit installed"
        );
        self.alerts.notify(format!(
            "CRUX TESTNET OCO INSTALLED\nOrder list: {}\nQuantity: {}\nEntry: {}",
            list.order_list_id, self.tracked_quantity, entry
        ));
        Ok(())
    }

    async fn cancel_protection(&mut self, config: &Config) -> Result<()> {
        if let Some(order_list_id) = self.protective_list_id.take() {
            self.client
                .cancel_protection(&config.symbol, order_list_id)
                .await?;
            info!(
                order_list_id,
                "protective order list canceled before strategy exit"
            );
        }
        Ok(())
    }

    async fn on_candle(
        &mut self,
        config: &Config,
        database: &Database,
        candle: &Candle,
        signal: Option<Signal>,
    ) -> Result<()> {
        let price = Decimal::from_f64_retain(candle.close).context("invalid candle price")?;
        let stale_entry = config.candle_is_stale(candle.close_time, now_ms()?);
        let exit_reason = self.entry_price.and_then(|entry| {
            let change = price / entry - Decimal::ONE;
            if self.protective_list_id.is_none()
                && change <= -Decimal::from_f64_retain(config.stop_loss)?
            {
                Some("stop loss")
            } else if self.protective_list_id.is_none()
                && change >= Decimal::from_f64_retain(config.take_profit)?
            {
                Some("take profit")
            } else if signal == Some(Signal::Sell) {
                Some("EMA crossover")
            } else {
                None
            }
        });
        let order_and_reason = if self.tracked_quantity.is_zero() && signal == Some(Signal::Buy) {
            if stale_entry {
                warn!(
                    close_time = candle.close_time,
                    stale_after_seconds = config.stale_data_seconds,
                    "stale Testnet candle cannot open a new position"
                );
                None
            } else {
                let risk = self
                    .refresh_risk(config, database, candle.close_time)
                    .await?;
                if risk.halted {
                    info!(
                        reason = risk.reason.as_deref().unwrap_or("risk limit reached"),
                        "Testnet buy signal blocked by risk circuit breaker"
                    );
                    None
                } else {
                    let account = self.client.account().await?;
                    let fraction = Decimal::from_f64_retain(config.position_fraction)
                        .context("invalid position fraction")?;
                    let cap = Decimal::from_f64_retain(config.max_order_quote)
                        .context("invalid max order quote")?;
                    let quote = self
                        .rules
                        .round_quote((account.free(&self.rules.quote_asset) * fraction).min(cap));
                    Some((
                        self.client
                            .market_buy(&config.symbol, quote, &self.rules)
                            .await?,
                        "EMA crossover",
                    ))
                }
            }
        } else if let Some(reason) = exit_reason {
            self.cancel_protection(config).await?;
            let account = self.client.account().await?;
            let quantity = self
                .tracked_quantity
                .min(account.free(&self.rules.base_asset));
            Some((
                self.client
                    .market_sell(&config.symbol, quantity, &self.rules)
                    .await?,
                reason,
            ))
        } else {
            None
        };

        if let Some((order, reason)) = order_and_reason {
            let was_buy = order.side == "BUY";
            let average_price = order.gross_quote / order.executed_base;
            let realized_pnl = if order.side == "BUY" {
                self.tracked_quantity = order.net_base;
                self.entry_price = Some(order.net_quote / order.net_base);
                Decimal::ZERO
            } else {
                let cost = self.entry_price.unwrap_or(average_price) * order.net_base;
                self.tracked_quantity = (self.tracked_quantity - order.net_base).max(Decimal::ZERO);
                if self.tracked_quantity < self.rules.min_quantity {
                    self.tracked_quantity = Decimal::ZERO;
                    self.entry_price = None;
                }
                order.net_quote - cost
            };
            let trade = Trade {
                timestamp: order.timestamp,
                side: if order.side == "BUY" { "BUY" } else { "SELL" },
                price: decimal_f64(average_price)?,
                quantity: decimal_f64(order.executed_base)?,
                fee: decimal_f64(order.quote_fee_equivalent)?,
                realized_pnl: decimal_f64(realized_pnl)?,
                reason,
            };
            database
                .record_exchange_order(
                    "testnet",
                    &config.symbol,
                    order.order_id,
                    &order.client_order_id,
                    &order.side,
                    &order.status,
                    trade.quantity,
                    decimal_f64(order.gross_quote)?,
                    order.timestamp,
                )
                .await?;
            Self::record_order_fills(database, &config.symbol, &order).await?;
            let inserted = database
                .record_exchange_trade_once("testnet", &config.symbol, order.order_id, &trade)
                .await?;
            if inserted {
                self.alerts.notify(format!(
                    "CRUX TESTNET {}\n{}\nOrder: {}\nPrice: {:.8}\nQuantity: {:.8}\nFee: {:.8}\nRealized PnL: {:.8}",
                    order.side,
                    reason,
                    order.order_id,
                    trade.price,
                    trade.quantity,
                    trade.fee,
                    trade.realized_pnl
                ));
            }
            info!(side = %order.side, order_id = order.order_id, status = %order.status,
                quantity = %order.executed_base, quote = %order.net_quote, "Testnet market order reconciled");
            if was_buy && let Err(error) = self.install_protection(config, database).await {
                warn!(%error, "protection placement failed; immediately flattening Testnet position");
                self.alerts.notify(format!(
                    "CRUX TESTNET CRITICAL\nOCO protection failed: {error}\nFlattening position immediately."
                ));
                let emergency = self
                    .client
                    .market_sell(&config.symbol, self.tracked_quantity, &self.rules)
                    .await?;
                let emergency_price = emergency.gross_quote / emergency.executed_base;
                let emergency_pnl = emergency.net_quote
                    - self.entry_price.unwrap_or(emergency_price) * emergency.net_base;
                let emergency_trade = Trade {
                    timestamp: emergency.timestamp,
                    side: "SELL",
                    price: decimal_f64(emergency_price)?,
                    quantity: decimal_f64(emergency.executed_base)?,
                    fee: decimal_f64(emergency.quote_fee_equivalent)?,
                    realized_pnl: decimal_f64(emergency_pnl)?,
                    reason: "protection failure",
                };
                self.tracked_quantity = Decimal::ZERO;
                self.entry_price = None;
                database
                    .record_exchange_order(
                        "testnet",
                        &config.symbol,
                        emergency.order_id,
                        &emergency.client_order_id,
                        &emergency.side,
                        &emergency.status,
                        decimal_f64(emergency.executed_base)?,
                        decimal_f64(emergency.gross_quote)?,
                        emergency.timestamp,
                    )
                    .await?;
                Self::record_order_fills(database, &config.symbol, &emergency).await?;
                let inserted = database
                    .record_exchange_trade_once(
                        "testnet",
                        &config.symbol,
                        emergency.order_id,
                        &emergency_trade,
                    )
                    .await?;
                if inserted {
                    self.alerts.notify(format!(
                        "CRUX TESTNET EMERGENCY SELL\nOrder: {}\nPrice: {:.8}\nQuantity: {:.8}\nRealized PnL: {:.8}",
                        emergency.order_id,
                        emergency_trade.price,
                        emergency_trade.quantity,
                        emergency_trade.realized_pnl
                    ));
                }
            }
        }

        let account = self.client.account().await?;
        let cash = account.free(&self.rules.quote_asset);
        let equity = cash + self.tracked_quantity * price;
        database
            .record_candle(&config.symbol, &config.interval, candle)
            .await?;
        database
            .save_state(
                candle.close_time,
                TraderState {
                    cash: decimal_f64(cash)?,
                    position_quantity: if self.tracked_quantity.is_zero() {
                        None
                    } else {
                        Some(decimal_f64(self.tracked_quantity)?)
                    },
                    entry_price: self.entry_price.map(decimal_f64).transpose()?,
                },
                candle.close,
                decimal_f64(equity)?,
            )
            .await?;
        self.refresh_risk(config, database, candle.close_time)
            .await?;
        Ok(())
    }

    async fn on_user_event(
        &mut self,
        config: &Config,
        database: &Database,
        event: UserEvent,
    ) -> Result<()> {
        if event.event_type != "executionReport"
            || event.symbol != config.symbol
            || !event.client_order_id.starts_with("crux")
        {
            return Ok(());
        }
        let previous = database
            .exchange_order_status("testnet", event.order_id)
            .await?;
        if matches!(
            previous.as_deref(),
            Some("FILLED" | "CANCELED" | "REJECTED" | "EXPIRED")
        ) && !matches!(
            event.status.as_str(),
            "FILLED" | "CANCELED" | "REJECTED" | "EXPIRED"
        ) {
            return Ok(());
        }
        let quantity = decimal_or_zero(Some(&event.cumulative_quantity));
        let gross_quote = decimal_or_zero(Some(&event.cumulative_quote));
        if event.status == "FILLED"
            && event.side == "SELL"
            && previous.as_deref() != Some("FILLED")
            && quantity > Decimal::ZERO
        {
            let reason = if event.client_order_id.starts_with("cruxS") {
                "exchange stop loss"
            } else if event.client_order_id.starts_with("cruxT") {
                "exchange take profit"
            } else {
                "exchange execution"
            };
            let response = self
                .client
                .query_order_by_id(&config.symbol, event.order_id)
                .await?;
            let fills = self
                .client
                .account_trades_for_order(&config.symbol, event.order_id)
                .await?;
            let order = ExecutedOrder::from_response_and_trades(response, fills, &self.rules)?;
            Self::record_order_fills(database, &config.symbol, &order).await?;
            let price = order.gross_quote / order.executed_base;
            self.apply_reconciled_sell(config, database, order, reason)
                .await?;
            let account = self.client.account().await?;
            let cash = account.free(&self.rules.quote_asset);
            let equity = cash + self.tracked_quantity * price;
            database
                .save_state(
                    event.transaction_time,
                    TraderState {
                        cash: decimal_f64(cash)?,
                        position_quantity: if self.tracked_quantity.is_zero() {
                            None
                        } else {
                            Some(decimal_f64(self.tracked_quantity)?)
                        },
                        entry_price: self.entry_price.map(decimal_f64).transpose()?,
                    },
                    decimal_f64(price)?,
                    decimal_f64(equity)?,
                )
                .await?;
            self.refresh_risk(config, database, event.transaction_time)
                .await?;
            info!(order_id = event.order_id, %reason, "authoritative Testnet execution report reconciled");
        }
        database
            .record_exchange_order(
                "testnet",
                &config.symbol,
                event.order_id,
                &event.client_order_id,
                &event.side,
                &event.status,
                decimal_f64(quantity)?,
                decimal_f64(gross_quote)?,
                event.transaction_time,
            )
            .await?;
        Ok(())
    }
}

pub async fn run_stream(
    config: &Config,
    strategy: &mut EmaCrossover,
    trader: &mut TestnetTrader,
    database: &Database,
) -> Result<StreamEnd> {
    let url = binance::market_stream_url(config);
    let (socket, _) = connect_async(&url)
        .await
        .with_context(|| format!("failed to connect to {url}"))?;
    info!(%url, "Testnet market stream connected");
    let (mut writer, mut reader) = socket.split();
    let (user_socket, _) = connect_async(&config.testnet_ws_api_base)
        .await
        .context("failed to connect to Testnet user-data WebSocket API")?;
    let (mut user_writer, mut user_reader) = user_socket.split();
    user_writer
        .send(Message::Text(trader.client.user_subscription()?.into()))
        .await?;
    let acknowledgement =
        tokio::time::timeout(std::time::Duration::from_secs(10), user_reader.next())
            .await
            .context("timed out subscribing to Testnet user-data stream")?
            .context("Testnet user-data stream closed before subscription")??;
    let acknowledgement: serde_json::Value = match acknowledgement {
        Message::Text(text) => serde_json::from_str(&text)?,
        _ => bail!("unexpected Testnet user-data subscription response"),
    };
    if acknowledgement
        .get("status")
        .and_then(serde_json::Value::as_i64)
        != Some(200)
    {
        bail!("Testnet user-data subscription rejected: {acknowledgement}");
    }
    info!("authoritative Testnet user-data stream subscribed");
    trader
        .reconcile(config, database)
        .await
        .context("failed to reconcile Testnet state after stream connection")?;
    trader.notify_recovered();
    loop {
        tokio::select! {
            _ = crate::shutdown::signal() => {
                let _ = writer.close().await;
                let _ = user_writer.close().await;
                return Ok(StreamEnd::Shutdown);
            },
            message = reader.next() => match message {
                Some(Ok(Message::Text(text))) => if let Some(candle) = binance::parse_closed_candle(&text)? {
                    let signal = strategy.on_close(candle.close);
                    let (fast, slow) = strategy.averages().unwrap_or_default();
                    debug!(price = candle.close, fast_ema = fast, slow_ema = slow, ?signal, "Testnet strategy evaluated");
                    trader.on_candle(config, database, &candle, signal).await?;
                },
                Some(Ok(Message::Ping(payload))) => writer.send(Message::Pong(payload)).await?,
                Some(Ok(Message::Close(_))) | None => return Ok(StreamEnd::Disconnected),
                Some(Ok(_)) => {},
                Some(Err(error)) => return Err(error.into()),
            },
            message = user_reader.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    let envelope: UserEnvelope = serde_json::from_str(&text)
                        .context("invalid Testnet user-data event")?;
                    if let Some(event) = envelope.event { trader.on_user_event(config, database, event).await?; }
                }
                Some(Ok(Message::Ping(payload))) => user_writer.send(Message::Pong(payload)).await?,
                Some(Ok(Message::Close(_))) | None => return Ok(StreamEnd::Disconnected),
                Some(Ok(_)) => {},
                Some(Err(error)) => return Err(error.into()),
            }
        }
    }
}

fn parse_required(value: &Option<String>, name: &str) -> Result<Decimal> {
    Decimal::from_str(
        value
            .as_deref()
            .with_context(|| format!("missing {name} filter"))?,
    )
    .map_err(Into::into)
}
fn decimal_or_zero(value: Option<&str>) -> Decimal {
    value
        .and_then(|value| Decimal::from_str(value).ok())
        .unwrap_or(Decimal::ZERO)
}
fn negative_one() -> i64 {
    -1
}
fn sign_query(secret: &str, query: &str) -> Result<String> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).context("invalid secret key")?;
    mac.update(query.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}
fn decimal_f64(value: Decimal) -> Result<f64> {
    value.to_f64().context("decimal is outside f64 range")
}
fn now_ms() -> Result<i64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64)
}
async fn binance_error(response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    match response.json::<BinanceError>().await {
        Ok(error) => anyhow::anyhow!(
            "Binance Testnet error {} (HTTP {}): {}",
            error.code,
            status,
            error.msg
        ),
        Err(error) => anyhow::anyhow!("Binance Testnet returned HTTP {status}: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> SymbolRules {
        SymbolRules {
            base_asset: "BTC".into(),
            quote_asset: "USDT".into(),
            quote_precision: 2,
            min_quantity: Decimal::from_str("0.00001").unwrap(),
            max_quantity: Decimal::from(100),
            step_size: Decimal::from_str("0.00001").unwrap(),
            min_notional: Decimal::from(5),
            tick_size: Decimal::from_str("0.01").unwrap(),
        }
    }

    #[test]
    fn rounds_market_quantity_down() {
        assert_eq!(
            rules().round_quantity(Decimal::from_str("0.1234567").unwrap()),
            Decimal::from_str("0.12345").unwrap()
        );
    }

    #[test]
    fn rejects_too_small_notional() {
        assert!(rules().validate_notional(Decimal::from(4)).is_err());
    }

    #[test]
    fn rounds_protective_prices_away_from_market() {
        let rules = rules();
        assert_eq!(
            rules.price_down(Decimal::from_str("99.999").unwrap()),
            Decimal::from_str("99.99").unwrap()
        );
        assert_eq!(
            rules.price_up(Decimal::from_str("100.001").unwrap()),
            Decimal::from_str("100.01").unwrap()
        );
    }

    #[test]
    fn matches_binance_hmac_example() {
        let query = "symbol=LTCBTC&side=BUY&type=LIMIT&timeInForce=GTC&quantity=1&price=0.1&recvWindow=5000&timestamp=1499827319559";
        let secret = "NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j";
        assert_eq!(
            sign_query(secret, query).unwrap(),
            "c8db56825ae71d6d79447849e617115f4a920fa2acdcab2b053c4b2838bd6b71"
        );
    }

    fn order(side: &str, order_id: i64) -> OrderResponse {
        OrderResponse {
            symbol: "BTCUSDT".into(),
            order_id,
            client_order_id: format!("crux{side}{order_id}"),
            order_list_id: -1,
            transact_time: Some(1_000),
            status: "FILLED".into(),
            side: side.into(),
            executed_qty: "0.1".into(),
            cummulative_quote_qty: "10".into(),
        }
    }

    #[test]
    fn exact_buy_fill_subtracts_base_commission() {
        let executed = ExecutedOrder::from_response_and_trades(
            order("BUY", 7),
            vec![AccountTrade {
                id: 70,
                order_id: 7,
                price: "100".into(),
                qty: "0.1".into(),
                quote_qty: "10".into(),
                commission: "0.0001".into(),
                commission_asset: "BTC".into(),
                time: 1_001,
                is_buyer: true,
            }],
            &rules(),
        )
        .unwrap();
        assert_eq!(executed.net_base, Decimal::from_str("0.0999").unwrap());
        assert_eq!(executed.net_quote, Decimal::from(10));
        assert_eq!(
            executed.quote_fee_equivalent,
            Decimal::from_str("0.01").unwrap()
        );
    }

    #[test]
    fn exact_sell_fill_subtracts_quote_commission() {
        let executed = ExecutedOrder::from_response_and_trades(
            order("SELL", 8),
            vec![AccountTrade {
                id: 80,
                order_id: 8,
                price: "100".into(),
                qty: "0.1".into(),
                quote_qty: "10".into(),
                commission: "0.01".into(),
                commission_asset: "USDT".into(),
                time: 1_001,
                is_buyer: false,
            }],
            &rules(),
        )
        .unwrap();
        assert_eq!(executed.net_base, Decimal::from_str("0.1").unwrap());
        assert_eq!(executed.net_quote, Decimal::from_str("9.99").unwrap());
        assert_eq!(
            executed.quote_fee_equivalent,
            Decimal::from_str("0.01").unwrap()
        );
    }
}

#[cfg(test)]
mod fault_tests;
