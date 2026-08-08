//! Client for the WhatsApp Go API (`aldinokemal/go-whatsapp-web-multidevice` v9).
//!
//! The API is unauthenticated and bound to 127.0.0.1. This module is the ONLY
//! place that talks to it, and it deliberately exposes no way to forward an
//! arbitrary path -- this service must never become a proxy onto an
//! unauthenticated API.

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::config::CONFIG;
use crate::errors::{AppError, AppResult};

/// The API caps `limit` at 100 and rejects anything larger with a validation
/// error rather than clamping.
pub const MAX_PAGE_LIMIT: u32 = 100;

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    #[allow(dead_code)]
    code: String,
    #[allow(dead_code)]
    #[serde(default)]
    message: String,
    results: T,
}

#[derive(Debug, Deserialize)]
struct MessagesResults {
    #[serde(default)]
    data: Vec<WaMessage>,
}

/// One message as the API returns it.
///
/// `content` is an empty string for media messages -- roughly a third of the
/// target chat. Those are still ingested (store raw first) and the triage gate
/// routes them to `ignored`.
#[derive(Debug, Clone, Deserialize)]
pub struct WaMessage {
    pub id: String,
    pub chat_jid: String,
    #[serde(default)]
    pub sender_jid: Option<String>,
    #[serde(default)]
    pub content: String,
    pub timestamp: DateTime<Utc>,
    #[serde(default)]
    pub is_from_me: bool,
    #[serde(default)]
    pub media_type: Option<String>,
}

#[derive(Clone)]
pub struct WhatsAppClient {
    http: reqwest::Client,
    base_url: String,
}

impl WhatsAppClient {
    pub fn from_config() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to build HTTP client"),
            base_url: CONFIG.whatsapp_api_url.trim_end_matches('/').to_string(),
        }
    }

    /// Fetch one page of messages for a chat, newest first.
    ///
    /// `start_time` is honoured by the API (verified against the live service), so
    /// the poller can ask for "everything since the cursor" instead of paging
    /// blindly through offsets.
    ///
    /// Note the response envelope's `pagination.total` is the chat's UNFILTERED
    /// total, not the count matching `start_time`, so it cannot be used to detect
    /// the last page. Callers must page until an empty batch.
    pub async fn list_messages(
        &self,
        chat_jid: &str,
        start_time: Option<DateTime<Utc>>,
        limit: u32,
        offset: u32,
    ) -> AppResult<Vec<WaMessage>> {
        let limit = limit.min(MAX_PAGE_LIMIT);

        // The JID contains '@' and must be path-encoded.
        let encoded_jid = urlencode(chat_jid);
        let mut url = format!(
            "{}/chat/{}/messages?limit={}&offset={}",
            self.base_url, encoded_jid, limit, offset
        );
        if let Some(ts) = start_time {
            url.push_str(&format!(
                "&start_time={}",
                urlencode(&ts.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            ));
        }

        let mut req = self.http.get(&url);
        // Unused today: the API has no auth. Wired so enabling it later is config.
        if let Some(token) = &CONFIG.whatsapp_api_token {
            req = req.bearer_auth(token);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| AppError::Upstream(format!("request failed: {e}")))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| AppError::Upstream(format!("failed to read body: {e}")))?;

        if !status.is_success() {
            return Err(AppError::Upstream(format!(
                "HTTP {status}: {}",
                truncate(&body, 300)
            )));
        }

        let parsed: Envelope<MessagesResults> = serde_json::from_str(&body).map_err(|e| {
            AppError::Upstream(format!(
                "unexpected response shape: {e} (body: {})",
                truncate(&body, 300)
            ))
        })?;

        Ok(parsed.results.data)
    }

    /// Liveness probe for `/readyz`. Cheap: asks for a single chat.
    pub async fn ping(&self) -> AppResult<()> {
        let url = format!("{}/chats?limit=1", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| AppError::Upstream(format!("request failed: {e}")))?;

        if resp.status().is_success() {
            Ok(())
        } else {
            Err(AppError::Upstream(format!("HTTP {}", resp.status())))
        }
    }
}

/// Minimal percent-encoding for path/query segments. Avoids pulling in a crate
/// for the handful of characters that actually occur in a JID or an RFC 3339
/// timestamp.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // Respect char boundaries; `s` may contain Arabic.
        let end = s
            .char_indices()
            .map(|(i, _)| i)
            .take_while(|i| *i <= max)
            .last()
            .unwrap_or(0);
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_escapes_jid_separators() {
        assert_eq!(
            urlencode("201280701070@s.whatsapp.net"),
            "201280701070%40s.whatsapp.net"
        );
        assert_eq!(
            urlencode("120363387047084332@g.us"),
            "120363387047084332%40g.us"
        );
    }

    #[test]
    fn urlencode_escapes_timestamp_colons() {
        assert_eq!(
            urlencode("2026-08-08T11:32:20Z"),
            "2026-08-08T11%3A32%3A20Z"
        );
    }

    #[test]
    fn truncate_does_not_split_arabic_chars() {
        let s = "تم خصم مبلغ EGP 15000.00 من حساب";
        let t = truncate(s, 10);
        // The point is that this does not panic on a multi-byte boundary.
        assert!(t.ends_with('…'));
    }

    /// The exact envelope shape returned by the live service, so a future version
    /// bump that changes field names fails here rather than in production.
    #[test]
    fn deserializes_real_response_shape() {
        let body = r#"{
          "code": "SUCCESS",
          "message": "Success get chat messages",
          "results": {
            "data": [{
              "id": "AC4B043147DEB40D107F510436F50E35",
              "chat_jid": "201280701070@s.whatsapp.net",
              "sender_jid": "201280701070@s.whatsapp.net",
              "content": "Transfer reference #ac8fc63f of EGP 85.00 has been debited",
              "timestamp": "2026-08-08T11:32:20Z",
              "is_from_me": false,
              "media_type": "",
              "created_at": "2026-08-08T11:32:20Z",
              "updated_at": "2026-08-08T11:32:21Z"
            }],
            "pagination": { "limit": 1, "offset": 0, "total": 729 }
          }
        }"#;

        let parsed: Envelope<MessagesResults> = serde_json::from_str(body).unwrap();
        let m = &parsed.results.data[0];
        assert_eq!(m.id, "AC4B043147DEB40D107F510436F50E35");
        assert!(!m.is_from_me);
        assert_eq!(m.timestamp.to_rfc3339(), "2026-08-08T11:32:20+00:00");
    }

    /// Media messages carry an empty `content` and must still deserialize.
    #[test]
    fn deserializes_media_message_with_empty_content() {
        let body = r#"{
          "code":"SUCCESS","results":{"data":[{
            "id":"X1","chat_jid":"c@g.us","sender_jid":"s@s.whatsapp.net",
            "content":"","timestamp":"2026-08-08T11:28:33Z",
            "is_from_me":false,"media_type":"image"}]}}"#;
        let parsed: Envelope<MessagesResults> = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.results.data[0].content, "");
        assert_eq!(parsed.results.data[0].media_type.as_deref(), Some("image"));
    }
}
