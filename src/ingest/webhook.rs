//! Webhook receiver for the WhatsApp Go API.
//!
//! The poller alone means up to `POLL_INTERVAL_SECS` of latency before a bank
//! message appears. The WhatsApp service can push instead
//! (`--webhook`, `--webhook-secret`), which makes ingestion effectively
//! immediate.
//!
//! **The poller stays running.** Webhooks are not a delivery guarantee: pushes
//! sent while this service is restarting or briefly unreachable are simply lost,
//! and nothing retries them. The poller's overlapping re-read is what makes a
//! missed push a non-event, so this is latency optimisation layered on top of a
//! guarantee, not a replacement for one. With both running, the interval can be
//! relaxed rather than tightened.
//!
//! Storing is the SAME path the poller uses. A second insert path would be a
//! second set of bugs, and the two would drift.

use actix_web::{web, HttpRequest, HttpResponse};
use chrono::{DateTime, Utc};
use log::{debug, info, warn};
use serde::Deserialize;
use sqlx::PgPool;

use crate::config::CONFIG;
use crate::errors::{AppError, AppResult};
use crate::ingest::poller;
use crate::ingest::whatsapp_client::WaMessage;

/// The payload shape go-whatsapp-web-multidevice posts for a message event.
///
/// Deliberately permissive: this is an off-the-shelf product whose payload may
/// gain fields, and a strict struct would start rejecting real messages after an
/// upgrade. Only what is needed to store the message is required.
#[derive(Debug, Deserialize)]
pub struct WebhookPayload {
    #[serde(default)]
    pub event: Option<String>,

    #[serde(default, alias = "message_id", alias = "id")]
    pub message_id: Option<String>,

    #[serde(default, alias = "chat_jid", alias = "from")]
    pub chat_jid: Option<String>,

    #[serde(default, alias = "sender_jid", alias = "sender")]
    pub sender_jid: Option<String>,

    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,

    #[serde(default)]
    pub is_from_me: Option<bool>,

    /// Body text. The product has used several names for this across versions.
    #[serde(default, alias = "text", alias = "body", alias = "content")]
    pub message: Option<serde_json::Value>,
}

/// Pull the body text out of whichever shape arrived.
///
/// The field is sometimes a bare string and sometimes an object with a `text`
/// key. An empty body is legitimate -- media messages have none -- so this
/// returns an empty string rather than failing.
fn extract_text(v: &Option<serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Object(map)) => map
            .get("text")
            .or_else(|| map.get("body"))
            .or_else(|| map.get("conversation"))
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// Constant-time comparison, so a wrong secret cannot be recovered by timing
/// how long the rejection takes.
fn secret_matches(provided: &str, expected: &str) -> bool {
    if provided.len() != expected.len() {
        return false;
    }
    provided
        .bytes()
        .zip(expected.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// Receive one message event.
///
/// Always answers 200 once the secret checks out, even when the message is
/// ignored or already stored. A webhook sender that sees an error will usually
/// retry or, worse, disable the endpoint; "I have this" is the honest answer to
/// a duplicate.
pub async fn receive(
    pool: web::Data<PgPool>,
    body: web::Json<WebhookPayload>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    // Unauthenticated endpoint by necessity -- the sender has no JWT -- so the
    // shared secret is the only gate. Refuse to run at all without one rather
    // than accepting anonymous writes into the ledger's source data.
    let Some(expected) = CONFIG.whatsapp_webhook_secret.as_deref() else {
        return Err(AppError::Forbidden);
    };

    let provided = req
        .headers()
        .get("X-Hub-Signature-256")
        .or_else(|| req.headers().get("X-Webhook-Secret"))
        .or_else(|| req.headers().get("Authorization"))
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim_start_matches("Bearer ").trim())
        .unwrap_or_default();

    if !secret_matches(provided, expected) {
        warn!("webhook rejected: bad or missing secret");
        return Err(AppError::Forbidden);
    }

    let p = body.into_inner();

    let (Some(id), Some(chat_jid)) = (p.message_id.clone(), p.chat_jid.clone()) else {
        // Not a message event (delivery receipts, presence, etc.). Acknowledge
        // and drop -- these are normal traffic, not errors.
        debug!("webhook ignored: event {:?} carried no message id", p.event);
        return Ok(HttpResponse::Ok().json(serde_json::json!({"status": "ignored"})));
    };

    // Only the chat we ingest. The webhook can be configured to send everything,
    // and the rest of someone's WhatsApp is none of this service's business.
    if chat_jid != CONFIG.target_chat_jid {
        debug!("webhook ignored: chat {chat_jid} is not the target");
        return Ok(HttpResponse::Ok().json(serde_json::json!({"status": "not_target_chat"})));
    }

    let message = WaMessage {
        id,
        chat_jid,
        sender_jid: p.sender_jid,
        content: extract_text(&p.message),
        // A push with no timestamp is treated as "now": it has just arrived.
        timestamp: p.timestamp.unwrap_or_else(Utc::now),
        is_from_me: p.is_from_me.unwrap_or(false),
        media_type: None,
    };

    let inserted = poller::ingest_messages(pool.get_ref(), &[message]).await?;

    if inserted > 0 {
        info!("webhook: 1 new message ingested");
    }

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "status": if inserted > 0 { "stored" } else { "duplicate" }
    })))
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    // No JWT guard: the sender is the WhatsApp service, which has no token. The
    // shared secret is the gate, and the handler refuses outright if none is
    // configured.
    cfg.route("/api/v1/ingest/webhook", web::post().to(receive));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_comparison_rejects_wrong_and_short_values() {
        assert!(secret_matches("hunter2", "hunter2"));
        assert!(!secret_matches("hunter3", "hunter2"));
        assert!(!secret_matches("", "hunter2"));
        assert!(!secret_matches("hunter2extra", "hunter2"));
    }

    #[test]
    fn body_text_is_read_from_every_shape_the_product_uses() {
        let s = |v: serde_json::Value| extract_text(&Some(v));
        assert_eq!(s(serde_json::json!("hello")), "hello");
        assert_eq!(s(serde_json::json!({"text": "hello"})), "hello");
        assert_eq!(s(serde_json::json!({"body": "hello"})), "hello");
        assert_eq!(s(serde_json::json!({"conversation": "hello"})), "hello");
    }

    #[test]
    fn missing_body_is_empty_not_an_error() {
        // Media messages legitimately have no text; they are still stored.
        assert_eq!(extract_text(&None), "");
        assert_eq!(extract_text(&Some(serde_json::json!({}))), "");
    }

    #[test]
    fn payload_parses_with_only_the_essential_fields() {
        let p: WebhookPayload = serde_json::from_str(
            r#"{"message_id":"ABC","chat_jid":"201280701070@s.whatsapp.net",
                "message":{"text":"EGP 100"},"timestamp":"2026-08-09T12:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(p.message_id.as_deref(), Some("ABC"));
        assert_eq!(extract_text(&p.message), "EGP 100");
    }

    /// An upgrade that adds fields must not start rejecting real messages.
    #[test]
    fn unknown_fields_are_tolerated() {
        let p: WebhookPayload = serde_json::from_str(
            r#"{"message_id":"A","chat_jid":"c","brand_new_field":123,"nested":{"x":1}}"#,
        )
        .unwrap();
        assert_eq!(p.message_id.as_deref(), Some("A"));
    }
}
