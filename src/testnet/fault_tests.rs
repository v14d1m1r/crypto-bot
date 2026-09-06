use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use reqwest::Client;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use super::*;
use crate::config::ExecutionMode;

struct MockExchange {
    account: Value,
    orders: Vec<Value>,
    trades: Vec<Value>,
    fail_account_requests: AtomicUsize,
    protection_installed: AtomicBool,
    oco_calls: AtomicUsize,
    market_calls: AtomicUsize,
}

impl MockExchange {
    fn new(account: Value, orders: Vec<Value>, trades: Vec<Value>) -> Self {
        Self {
            account,
            orders,
            trades,
            fail_account_requests: AtomicUsize::new(0),
            protection_installed: AtomicBool::new(false),
            oco_calls: AtomicUsize::new(0),
            market_calls: AtomicUsize::new(0),
        }
    }
}

async fn account(State(state): State<Arc<MockExchange>>) -> Response {
    if state
        .fail_account_requests
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
            remaining.checked_sub(1)
        })
        .is_ok()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"code": -1000, "msg": "injected account failure"})),
        )
            .into_response();
    }
    Json(state.account.clone()).into_response()
}

async fn all_orders(State(state): State<Arc<MockExchange>>) -> Json<Vec<Value>> {
    Json(state.orders.clone())
}

async fn account_trades(State(state): State<Arc<MockExchange>>) -> Json<Vec<Value>> {
    Json(state.trades.clone())
}

async fn query_order(State(state): State<Arc<MockExchange>>) -> Json<Value> {
    Json(state.orders.first().cloned().unwrap_or_else(|| json!({})))
}

async fn place_market(State(state): State<Arc<MockExchange>>) -> Json<Value> {
    state.market_calls.fetch_add(1, Ordering::SeqCst);
    Json(state.orders.first().cloned().unwrap_or_else(|| json!({})))
}

async fn open_orders(State(state): State<Arc<MockExchange>>) -> Json<Vec<Value>> {
    if state.protection_installed.load(Ordering::SeqCst) {
        Json(vec![json!({
            "symbol": "BTCUSDT", "orderId": 901, "clientOrderId": "cruxT901",
            "orderListId": 999, "status": "NEW", "side": "SELL",
            "executedQty": "0", "cummulativeQuoteQty": "0"
        })])
    } else {
        Json(Vec::new())
    }
}

async fn average_price() -> Json<Value> {
    Json(json!({"price": "100"}))
}

async fn place_oco(State(state): State<Arc<MockExchange>>) -> Json<Value> {
    state.oco_calls.fetch_add(1, Ordering::SeqCst);
    state.protection_installed.store(true, Ordering::SeqCst);
    Json(json!({
        "orderListId": 999,
        "transactionTime": 2_000,
        "listOrderStatus": "EXECUTING",
        "orders": [
            {"orderId": 901, "clientOrderId": "cruxT901"},
            {"orderId": 902, "clientOrderId": "cruxS902"}
        ]
    }))
}

async fn spawn_mock(state: Arc<MockExchange>) -> (String, JoinHandle<()>) {
    let app = Router::new()
        .route("/api/v3/account", get(account))
        .route("/api/v3/allOrders", get(all_orders))
        .route("/api/v3/myTrades", get(account_trades))
        .route("/api/v3/order", get(query_order).post(place_market))
        .route("/api/v3/openOrders", get(open_orders))
        .route("/api/v3/avgPrice", get(average_price))
        .route("/api/v3/orderList/oco", post(place_oco))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{address}"), task)
}

fn rules() -> SymbolRules {
    SymbolRules {
        base_asset: "BTC".into(),
        quote_asset: "USDT".into(),
        quote_precision: 2,
        min_quantity: Decimal::new(1, 5),
        max_quantity: Decimal::from(100),
        step_size: Decimal::new(1, 5),
        min_notional: Decimal::from(5),
        tick_size: Decimal::new(1, 2),
    }
}

fn config() -> Config {
    let mut config = Config::default_for_test();
    config.mode = ExecutionMode::Testnet;
    config
}

fn trader(rest_base: String, quantity: Decimal, entry: Option<Decimal>) -> TestnetTrader {
    TestnetTrader {
        client: TestnetClient {
            http: Client::builder().build().unwrap(),
            api_key: "fault-test-key".into(),
            secret_key: "fault-test-secret".into(),
            rest_base,
            time_offset_ms: 0,
        },
        rules: rules(),
        tracked_quantity: quantity,
        entry_price: entry,
        protective_list_id: None,
        alerts: TelegramAlerter::default(),
        last_risk_alert: None,
        last_reconnect_alert_ms: None,
        connection_warning_active: false,
    }
}

fn account_json(base_free: &str, quote_free: &str) -> Value {
    json!({"balances": [
        {"asset": "BTC", "free": base_free, "locked": "0"},
        {"asset": "USDT", "free": quote_free, "locked": "0"}
    ]})
}

#[tokio::test]
async fn fault_harness_retains_recovered_dust_without_order_attempts() {
    let state = Arc::new(MockExchange::new(
        account_json("0.00001", "1000"),
        vec![json!({
            "symbol": "BTCUSDT", "orderId": 99, "clientOrderId": "cruxB99",
            "orderListId": -1, "status": "FILLED", "side": "BUY",
            "executedQty": "0.00001", "cummulativeQuoteQty": "0.8049145"
        })],
        vec![json!({
            "id": 900, "orderId": 99, "price": "80491.45", "qty": "0.00001",
            "quoteQty": "0.8049145", "commission": "0", "commissionAsset": "BTC",
            "time": 1201, "isBuyer": true
        })],
    ));
    let (base, server) = spawn_mock(state.clone()).await;
    let database = Database::connect("sqlite::memory:").await.unwrap();
    let mut trader = trader(base, Decimal::ZERO, None);
    let config = config();
    for _ in 0..2 {
        trader.reconcile(&config, &database).await.unwrap();
        trader
            .on_candle(
                &config,
                &database,
                &Candle {
                    close_time: now_ms().unwrap(),
                    close: 80491.45,
                },
                Some(Signal::Sell),
            )
            .await
            .unwrap();
    }
    assert_eq!(trader.tracked_quantity, Decimal::new(1, 5));
    assert_eq!(trader.entry_price, Some(Decimal::new(8049145, 2)));
    let saved = database.load_trader_state().await.unwrap().unwrap();
    assert_eq!(saved.position_quantity, Some(0.00001));
    assert_eq!(saved.entry_price, Some(80491.45));
    assert_eq!(database.recent_exchange_fills(10).await.unwrap().len(), 1);
    assert_eq!(state.oco_calls.load(Ordering::SeqCst), 0);
    assert_eq!(state.market_calls.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn fault_harness_strategy_buy_combines_dust_and_installs_protection() {
    let state = Arc::new(MockExchange::new(
        account_json("0.108", "990"),
        vec![json!({
            "symbol": "BTCUSDT", "orderId": 99, "clientOrderId": "cruxB99",
            "orderListId": -1, "transactTime": 1201, "status": "FILLED", "side": "BUY",
            "executedQty": "0.1", "cummulativeQuoteQty": "10"
        })],
        vec![json!({
            "id": 900, "orderId": 99, "price": "100", "qty": "0.1",
            "quoteQty": "10", "commission": "0", "commissionAsset": "BTC",
            "time": 1201, "isBuyer": true
        })],
    ));
    let (base, server) = spawn_mock(state.clone()).await;
    let database = Database::connect("sqlite::memory:").await.unwrap();
    let mut trader = trader(base, Decimal::new(8, 3), Some(Decimal::from(80)));
    trader
        .on_candle(
            &config(),
            &database,
            &Candle {
                close_time: now_ms().unwrap(),
                close: 100.0,
            },
            Some(Signal::Buy),
        )
        .await
        .unwrap();
    assert_eq!(trader.tracked_quantity, Decimal::new(108, 3));
    assert_eq!(
        trader.entry_price,
        Some(Decimal::new(1064, 2) / Decimal::new(108, 3))
    );
    assert_eq!(trader.protective_list_id, Some(999));
    assert_eq!(state.market_calls.load(Ordering::SeqCst), 1);
    assert_eq!(state.oco_calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn fault_harness_completed_sale_preserves_dust_and_clears_protection() {
    let database = Database::connect("sqlite::memory:").await.unwrap();
    let mut trader = trader(
        String::new(),
        Decimal::new(108, 3),
        Some(Decimal::from(100)),
    );
    trader.protective_list_id = Some(999);
    let order = ExecutedOrder {
        order_id: 901,
        client_order_id: "cruxT901".into(),
        timestamp: 2000,
        side: "SELL".into(),
        status: "FILLED".into(),
        executed_base: Decimal::new(1, 1),
        net_base: Decimal::new(1, 1),
        gross_quote: Decimal::from(10),
        net_quote: Decimal::from(10),
        quote_fee_equivalent: Decimal::ZERO,
        fills: Vec::new(),
    };
    trader
        .apply_reconciled_sell(&config(), &database, order, "test residual")
        .await
        .unwrap();
    assert_eq!(trader.tracked_quantity, Decimal::new(8, 3));
    assert_eq!(trader.entry_price, Some(Decimal::from(100)));
    assert_eq!(trader.protective_list_id, None);
}

#[tokio::test]
async fn fault_harness_recovers_missed_protective_fill_once() {
    let state = Arc::new(MockExchange::new(
        account_json("0", "1009.99"),
        vec![json!({
            "symbol": "BTCUSDT", "orderId": 77, "clientOrderId": "cruxT77",
            "orderListId": 7, "transactTime": 1_001, "status": "FILLED", "side": "SELL",
            "executedQty": "0.1", "cummulativeQuoteQty": "10"
        })],
        vec![json!({
            "symbol": "BTCUSDT", "id": 700, "orderId": 77, "price": "100",
            "qty": "0.1", "quoteQty": "10", "commission": "0.01",
            "commissionAsset": "USDT", "time": 1_001, "isBuyer": false,
            "isMaker": false, "isBestMatch": true
        })],
    ));
    let (base, server) = spawn_mock(state).await;
    let database = Database::connect("sqlite::memory:").await.unwrap();
    database
        .record_exchange_order(
            "testnet", "BTCUSDT", 77, "cruxT77", "SELL", "NEW", 0.0, 0.0, 900,
        )
        .await
        .unwrap();
    let mut trader = trader(base, Decimal::new(1, 1), Some(Decimal::from(90)));
    let config = config();

    trader.reconcile(&config, &database).await.unwrap();
    trader.reconcile(&config, &database).await.unwrap();

    assert_eq!(trader.tracked_quantity, Decimal::ZERO);
    assert_eq!(database.recent_trades(10).await.unwrap().len(), 1);
    assert_eq!(database.recent_exchange_fills(10).await.unwrap().len(), 1);
    assert_eq!(
        database
            .exchange_order_status("testnet", 77)
            .await
            .unwrap()
            .as_deref(),
        Some("FILLED")
    );
    server.abort();
}

#[tokio::test]
async fn fault_harness_restores_missing_oco_without_duplicates() {
    let state = Arc::new(MockExchange::new(
        account_json("0.1", "990"),
        Vec::new(),
        Vec::new(),
    ));
    let (base, server) = spawn_mock(state.clone()).await;
    let database = Database::connect("sqlite::memory:").await.unwrap();
    let mut trader = trader(base, Decimal::new(1, 1), Some(Decimal::from(100)));
    let config = config();

    trader.reconcile(&config, &database).await.unwrap();
    trader.reconcile(&config, &database).await.unwrap();

    assert_eq!(trader.protective_list_id, Some(999));
    assert_eq!(state.oco_calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn fault_harness_retries_cleanly_after_transient_rest_failure() {
    let state = Arc::new(MockExchange::new(
        account_json("0", "1000"),
        Vec::new(),
        Vec::new(),
    ));
    state.fail_account_requests.store(1, Ordering::SeqCst);
    let (base, server) = spawn_mock(state).await;
    let database = Database::connect("sqlite::memory:").await.unwrap();
    let mut trader = trader(base, Decimal::ZERO, None);
    let config = config();

    assert!(trader.reconcile(&config, &database).await.is_err());
    trader.reconcile(&config, &database).await.unwrap();

    assert_eq!(trader.tracked_quantity, Decimal::ZERO);
    assert!(database.status().await.unwrap().is_some());
    assert!(
        !database
            .risk_status("testnet", "BTCUSDT")
            .await
            .unwrap()
            .unwrap()
            .halted
    );
    server.abort();
}

#[tokio::test]
async fn fault_harness_recovers_state_after_crash_boundary() {
    let state = Arc::new(MockExchange::new(
        account_json("0", "1009.99"),
        vec![json!({
            "symbol": "BTCUSDT", "orderId": 88, "clientOrderId": "cruxS88",
            "orderListId": 8, "transactTime": 1_101, "status": "FILLED", "side": "SELL",
            "executedQty": "0.1", "cummulativeQuoteQty": "10"
        })],
        vec![json!({
            "symbol": "BTCUSDT", "id": 800, "orderId": 88, "price": "100",
            "qty": "0.1", "quoteQty": "10", "commission": "0.01",
            "commissionAsset": "USDT", "time": 1_101, "isBuyer": false,
            "isMaker": false, "isBestMatch": true
        })],
    ));
    let (base, server) = spawn_mock(state).await;
    let database = Database::connect("sqlite::memory:").await.unwrap();
    database
        .record_exchange_order(
            "testnet", "BTCUSDT", 88, "cruxS88", "SELL", "NEW", 0.0, 0.0, 1_000,
        )
        .await
        .unwrap();
    database
        .record_exchange_trade_once(
            "testnet",
            "BTCUSDT",
            88,
            &Trade {
                timestamp: 1_101,
                side: "SELL",
                price: 100.0,
                quantity: 0.1,
                fee: 0.01,
                realized_pnl: 1.0,
                reason: "pre-crash committed trade",
            },
        )
        .await
        .unwrap();
    let mut trader = trader(base, Decimal::new(1, 1), Some(Decimal::from(90)));
    let config = config();

    trader.reconcile(&config, &database).await.unwrap();

    assert_eq!(trader.tracked_quantity, Decimal::ZERO);
    assert_eq!(database.recent_trades(10).await.unwrap().len(), 1);
    assert_eq!(
        database
            .exchange_order_status("testnet", 88)
            .await
            .unwrap()
            .as_deref(),
        Some("FILLED")
    );
    server.abort();
}

#[tokio::test]
async fn fault_harness_reconstructs_buy_filled_before_crash() {
    let state = Arc::new(MockExchange::new(
        account_json("0.0999", "990"),
        vec![json!({
            "symbol": "BTCUSDT", "orderId": 99, "clientOrderId": "cruxB99",
            "orderListId": -1, "transactTime": 1_201, "status": "FILLED", "side": "BUY",
            "executedQty": "0.1", "cummulativeQuoteQty": "10"
        })],
        vec![json!({
            "symbol": "BTCUSDT", "id": 900, "orderId": 99, "price": "100",
            "qty": "0.1", "quoteQty": "10", "commission": "0.0001",
            "commissionAsset": "BTC", "time": 1_201, "isBuyer": true,
            "isMaker": false, "isBestMatch": true
        })],
    ));
    let (base, server) = spawn_mock(state.clone()).await;
    let database = Database::connect("sqlite::memory:").await.unwrap();
    let mut trader = trader(base, Decimal::ZERO, None);
    let config = config();

    trader.reconcile(&config, &database).await.unwrap();

    assert_eq!(trader.tracked_quantity, Decimal::new(999, 4));
    assert_eq!(
        trader.entry_price.unwrap(),
        Decimal::from(10) / Decimal::new(999, 4)
    );
    assert_eq!(trader.protective_list_id, Some(999));
    assert_eq!(state.oco_calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn fault_harness_clears_stale_position_after_exchange_reset() {
    let state = Arc::new(MockExchange::new(
        account_json("0", "1000"),
        Vec::new(),
        Vec::new(),
    ));
    let (base, server) = spawn_mock(state.clone()).await;
    let database = Database::connect("sqlite::memory:").await.unwrap();
    let mut trader = trader(base, Decimal::new(1, 1), Some(Decimal::from(100)));
    let config = config();

    trader.reconcile(&config, &database).await.unwrap();

    assert_eq!(trader.tracked_quantity, Decimal::ZERO);
    assert_eq!(trader.entry_price, None);
    assert_eq!(trader.protective_list_id, None);
    assert_eq!(state.oco_calls.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn fault_harness_ignores_duplicate_execution_reports() {
    let state = Arc::new(MockExchange::new(
        account_json("0", "1009.99"),
        vec![json!({
            "symbol": "BTCUSDT", "orderId": 111, "clientOrderId": "cruxT111",
            "orderListId": 11, "transactTime": 1_301, "status": "FILLED", "side": "SELL",
            "executedQty": "0.1", "cummulativeQuoteQty": "10"
        })],
        vec![json!({
            "symbol": "BTCUSDT", "id": 1_100, "orderId": 111, "price": "100",
            "qty": "0.1", "quoteQty": "10", "commission": "0.01",
            "commissionAsset": "USDT", "time": 1_301, "isBuyer": false,
            "isMaker": false, "isBestMatch": true
        })],
    ));
    let (base, server) = spawn_mock(state).await;
    let database = Database::connect("sqlite::memory:").await.unwrap();
    database
        .record_exchange_order(
            "testnet", "BTCUSDT", 111, "cruxT111", "SELL", "NEW", 0.0, 0.0, 1_200,
        )
        .await
        .unwrap();
    let mut trader = trader(base, Decimal::new(1, 1), Some(Decimal::from(90)));
    let config = config();
    let event = || UserEvent {
        event_type: "executionReport".into(),
        symbol: "BTCUSDT".into(),
        client_order_id: "cruxT111".into(),
        side: "SELL".into(),
        status: "FILLED".into(),
        order_id: 111,
        cumulative_quantity: "0.1".into(),
        cumulative_quote: "10".into(),
        transaction_time: 1_301,
    };

    trader
        .on_user_event(&config, &database, event())
        .await
        .unwrap();
    trader
        .on_user_event(&config, &database, event())
        .await
        .unwrap();

    assert_eq!(trader.tracked_quantity, Decimal::ZERO);
    assert_eq!(database.recent_trades(10).await.unwrap().len(), 1);
    assert_eq!(database.recent_exchange_fills(10).await.unwrap().len(), 1);
    server.abort();
}
