use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::config::Config;

#[derive(Clone, Default)]
pub struct TelegramAlerter {
    inner: Option<Arc<TelegramInner>>,
}

struct TelegramInner {
    http: Client,
    endpoint: String,
    chat_id: String,
    pending: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

#[derive(Serialize)]
struct SendMessage<'a> {
    chat_id: &'a str,
    text: &'a str,
    disable_web_page_preview: bool,
}

#[derive(Deserialize)]
struct TelegramResponse {
    ok: bool,
    description: Option<String>,
}

impl TelegramAlerter {
    pub fn from_config(config: &Config) -> Self {
        let Some(token) = config.telegram_bot_token.as_deref() else {
            return Self::default();
        };
        let Some(chat_id) = config.telegram_chat_id.as_deref() else {
            return Self::default();
        };
        let http = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("valid Telegram HTTP client configuration");
        Self {
            inner: Some(Arc::new(TelegramInner {
                http,
                endpoint: format!(
                    "{}/bot{token}/sendMessage",
                    config.telegram_api_base.trim_end_matches('/')
                ),
                chat_id: chat_id.into(),
                pending: Mutex::new(Vec::new()),
            })),
        }
    }

    pub fn enabled(&self) -> bool {
        self.inner.is_some()
    }

    pub async fn send(&self, text: &str) -> Result<()> {
        let inner = self
            .inner
            .as_deref()
            .context("Telegram alerts require TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID")?;
        if text.is_empty() || text.chars().count() > 4096 {
            bail!("Telegram alert text must contain 1 to 4096 characters");
        }
        let response = inner
            .http
            .post(&inner.endpoint)
            .json(&SendMessage {
                chat_id: &inner.chat_id,
                text,
                disable_web_page_preview: true,
            })
            .send()
            .await
            .map_err(|_| anyhow!("failed to reach Telegram Bot API"))?;
        let status = response.status();
        let response: TelegramResponse = response
            .json()
            .await
            .context("invalid Telegram Bot API response")?;
        if !status.is_success() || !response.ok {
            bail!(
                "Telegram rejected alert (HTTP {status}): {}",
                response.description.as_deref().unwrap_or("unknown error")
            );
        }
        Ok(())
    }

    pub fn notify(&self, text: impl Into<String>) {
        let Some(inner) = self.inner.as_ref() else {
            return;
        };
        let alerter = self.clone();
        let text = text.into();
        let task = tokio::spawn(async move {
            if let Err(error) = alerter.send(&text).await {
                warn!(%error, "Telegram alert delivery failed");
            }
        });
        let mut pending = inner.pending.lock().expect("Telegram alert queue poisoned");
        pending.retain(|task| !task.is_finished());
        pending.push(task);
    }

    pub async fn flush(&self) {
        let Some(inner) = self.inner.as_ref() else {
            return;
        };
        let pending = {
            let mut pending = inner.pending.lock().expect("Telegram alert queue poisoned");
            std::mem::take(&mut *pending)
        };
        for task in pending {
            let _ = task.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{Json, Router, extract::State, routing::post};
    use serde_json::{Value, json};

    use super::*;

    async fn capture(
        State(messages): State<Arc<Mutex<Vec<Value>>>>,
        Json(payload): Json<Value>,
    ) -> Json<Value> {
        messages.lock().unwrap().push(payload);
        Json(json!({"ok": true, "result": {"message_id": 1}}))
    }

    #[tokio::test]
    async fn sends_expected_bot_api_payload() {
        let messages = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/botsecret/sendMessage", post(capture))
            .with_state(messages.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut config = Config::default_for_test();
        config.telegram_bot_token = Some("secret".into());
        config.telegram_chat_id = Some("-12345".into());
        config.telegram_api_base = format!("http://{address}");
        let alerter = TelegramAlerter::from_config(&config);

        alerter.send("test alert").await.unwrap();

        let messages = messages.lock().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["chat_id"], "-12345");
        assert_eq!(messages[0]["text"], "test alert");
        assert_eq!(messages[0]["disable_web_page_preview"], true);
        server.abort();
    }

    #[tokio::test]
    async fn flush_waits_for_queued_alerts() {
        let messages = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/botsecret/sendMessage", post(capture))
            .with_state(messages.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut config = Config::default_for_test();
        config.telegram_bot_token = Some("secret".into());
        config.telegram_chat_id = Some("123".into());
        config.telegram_api_base = format!("http://{address}");
        let alerter = TelegramAlerter::from_config(&config);

        alerter.notify("queued alert");
        alerter.flush().await;

        assert_eq!(messages.lock().unwrap().len(), 1);
        server.abort();
    }
}
