use std::sync::Arc;

use anyhow::Result;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderValue, Method, StatusCode, header},
    routing::get,
};
use serde_json::{Value, json};
use tower_http::cors::CorsLayer;
use tracing::info;

use crate::{config::Config, database::Database};

#[derive(Clone)]
struct ApiState {
    database: Database,
    config: Config,
}
type SharedState = Arc<ApiState>;
type ApiResult = Result<Json<Value>, (StatusCode, String)>;

pub async fn serve(database: Database, config: Config) -> Result<()> {
    let address = config.api_address.clone();
    let allowed_origin = config.cors_origin.parse::<HeaderValue>()?;
    let state = Arc::new(ApiState { database, config });
    let cors = CorsLayer::new()
        .allow_origin(allowed_origin)
        .allow_methods([Method::GET])
        .allow_headers([header::CONTENT_TYPE]);
    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/status", get(status))
        .route("/api/trades", get(trades))
        .route("/api/fills", get(fills))
        .route("/api/equity", get(equity))
        .route("/api/candles", get(candles))
        .layer(cors)
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(&address).await?;
    info!(%address, "dashboard API listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health(State(state): State<SharedState>) -> (StatusCode, Json<Value>) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default();
    let latest_candle = match state
        .database
        .latest_candle_time(&state.config.symbol, &state.config.interval)
        .await
    {
        Ok(value) => value,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"ok": false, "reason": "database query failed"})),
            );
        }
    };
    let Some(latest_candle) = latest_candle else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "reason": "no closed candle recorded"})),
        );
    };
    let age_ms = now.saturating_sub(latest_candle);
    let stale = state.config.candle_is_stale(latest_candle, now);
    let risk = state
        .database
        .risk_status(&state.config.mode.to_string(), &state.config.symbol)
        .await
        .ok()
        .flatten();
    let response = Json(json!({
        "ok": !stale,
        "ready_for_entries": !stale && !risk.as_ref().is_some_and(|risk| risk.halted),
        "reason": if stale { Some("market data is stale") } else { None },
        "last_candle_time": latest_candle,
        "candle_age_seconds": age_ms / 1_000,
        "stale_after_seconds": state.config.stale_data_seconds,
        "risk_halted": risk.as_ref().is_some_and(|risk| risk.halted),
    }));
    if stale {
        (StatusCode::SERVICE_UNAVAILABLE, response)
    } else {
        (StatusCode::OK, response)
    }
}

async fn status(State(state): State<SharedState>) -> ApiResult {
    let status = state.database.status().await.map_err(internal)?;
    let environment = state.config.mode.to_string();
    let risk = state
        .database
        .risk_status(&environment, &state.config.symbol)
        .await
        .map_err(internal)?;
    Ok(Json(json!({
        "bot": status,
        "risk": risk,
        "config": { "symbol": state.config.symbol, "interval": state.config.interval,
            "fast_ema": state.config.fast_ema, "slow_ema": state.config.slow_ema,
            "starting_cash": state.config.starting_cash, "mode": environment,
            "max_daily_loss_quote": state.config.max_daily_loss_quote,
            "max_entries_per_day": state.config.max_entries_per_day,
            "max_consecutive_losses": state.config.max_consecutive_losses }
    })))
}

async fn trades(State(state): State<SharedState>) -> ApiResult {
    Ok(Json(json!(
        state.database.recent_trades(50).await.map_err(internal)?
    )))
}

async fn fills(State(state): State<SharedState>) -> ApiResult {
    Ok(Json(json!(
        state
            .database
            .recent_exchange_fills(100)
            .await
            .map_err(internal)?
    )))
}

async fn equity(State(state): State<SharedState>) -> ApiResult {
    Ok(Json(json!(
        state.database.recent_equity(200).await.map_err(internal)?
    )))
}

async fn candles(State(state): State<SharedState>) -> ApiResult {
    Ok(Json(json!(
        state
            .database
            .recent_candles(&state.config.symbol, &state.config.interval, 200)
            .await
            .map_err(internal)?
    )))
}

fn internal(error: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binance::Candle;

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    #[tokio::test]
    async fn health_requires_a_recent_candle() {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        let config = Config::default_for_test();
        let state = Arc::new(ApiState {
            database: database.clone(),
            config: config.clone(),
        });
        let (status, body) = health(State(state.clone())).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.0["ok"], false);

        let timestamp = now_ms();
        database
            .record_candle(
                &config.symbol,
                &config.interval,
                &Candle {
                    close_time: timestamp,
                    close: 100.0,
                },
            )
            .await
            .unwrap();
        let (status, body) = health(State(state)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.0["ok"], true);
    }

    #[tokio::test]
    async fn health_rejects_stale_market_data() {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        let mut config = Config::default_for_test();
        config.stale_data_seconds = 1;
        database
            .record_candle(
                &config.symbol,
                &config.interval,
                &Candle {
                    close_time: now_ms() - 2_000,
                    close: 100.0,
                },
            )
            .await
            .unwrap();
        let state = Arc::new(ApiState { database, config });
        let (status, body) = health(State(state)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.0["reason"], "market data is stale");
    }
}
