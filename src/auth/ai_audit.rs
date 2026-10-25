//! The audit trail for writes the AI advisor makes to apex-rust's own data.
//!
//! An approved suggestion reaches the bank-SMS routes over loopback with the
//! approver's token plus `X-Via-AI` and `X-AI-Suggestion-Id`, exactly as it
//! reaches FalconGo and maint-rust. Each such request gets one row in
//! `advisor.ai_audit_log` naming the human, the AI channel and the suggestion,
//! so "who changed this, and was it the AI" has the same answer in every
//! service. The row is written before the handler runs and completed with the
//! status after, so an AI write can never land unrecorded.
//!
//! Requests without the header are untouched.

use actix_web::body::MessageBody;
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::http::header::HeaderMap;
use actix_web::middleware::Next;
use actix_web::{web, Error, HttpMessage};
use sqlx::PgPool;
use uuid::Uuid;

use super::permissions::{level, AuthContext};

/// The `X-Via-AI` value, if any: a short channel name such as `advisor`.
/// Anything that is not a plain token is refused rather than stored.
pub fn via_ai(headers: &HeaderMap) -> Option<String> {
    let v = headers.get("X-Via-AI")?.to_str().ok()?.trim();
    let ok = !v.is_empty()
        && v.len() <= 40
        && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    // A malformed value still marks the request as AI-made; it is just not
    // trusted as a label.
    Some(if ok { v.to_string() } else { "unknown".to_string() })
}

pub fn suggestion_id(headers: &HeaderMap) -> Option<Uuid> {
    headers
        .get("X-AI-Suggestion-Id")?
        .to_str()
        .ok()
        .and_then(|s| Uuid::parse_str(s.trim()).ok())
}

/// Middleware for the authenticated scopes. It must sit OUTSIDE `JwtAuth`
/// (which resolves the caller), so it decodes nothing itself: the open row is
/// written once the inner service has authenticated the request, and the
/// handler only runs after that row exists.
pub async fn record(
    req: ServiceRequest,
    next: Next<impl MessageBody + 'static>,
) -> Result<ServiceResponse<impl MessageBody>, Error> {
    let Some(via) = via_ai(req.headers()) else {
        return next.call(req).await;
    };
    let pool = req
        .app_data::<web::Data<PgPool>>()
        .cloned()
        .ok_or_else(|| actix_web::error::ErrorInternalServerError("no database pool"))?;
    let suggestion = suggestion_id(req.headers());
    let method = req.method().to_string();
    let path = req.path().to_string();

    // Open the row first. The user is not known yet (JwtAuth runs inside
    // this), so it is filled in from the response's request extensions.
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO advisor.ai_audit_log (via, suggestion_id, method, path)
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(&via)
    .bind(suggestion)
    .bind(&method)
    .bind(&path)
    .fetch_one(pool.get_ref())
    .await
    .map_err(|e| {
        log::error!("ai audit: could not open a row for {method} {path}: {e}");
        actix_web::error::ErrorServiceUnavailable("the AI audit log is unavailable; nothing was changed")
    })?;

    let res = next.call(req).await?;

    let ctx = res.request().extensions().get::<AuthContext>().cloned();
    let status = res.status().as_u16() as i32;
    let close = sqlx::query(
        "UPDATE advisor.ai_audit_log
            SET user_id = $2, permission = $3, status = $4, finished_at = now()
          WHERE id = $1",
    )
    .bind(id)
    .bind(ctx.as_ref().map(|c| c.user_id))
    .bind(ctx.as_ref().map(|c| c.permission))
    .bind(status)
    .execute(pool.get_ref())
    .await;
    if let Err(e) = close {
        // The change (if any) is made and the row exists without its status;
        // the suggestion row in advisor.suggestions still records the result.
        log::error!("ai audit: could not close row {id}: {e}");
    }
    if let Some(c) = &ctx {
        if c.permission < level::ADMIN {
            // JwtAuth::at_least(4) already refused it; this is belt and braces
            // for a route mounted without the floor.
            log::error!("ai audit: AI write by permission {} on {path}", c.permission);
        }
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::http::header::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn the_channel_is_read_and_sanitised() {
        assert_eq!(via_ai(&headers(&[])), None);
        assert_eq!(via_ai(&headers(&[("X-Via-AI", "advisor")])).as_deref(), Some("advisor"));
        assert_eq!(
            via_ai(&headers(&[("X-Via-AI", "drop table; --")])).as_deref(),
            Some("unknown")
        );
    }

    #[test]
    fn a_bad_suggestion_id_is_dropped() {
        assert!(suggestion_id(&headers(&[("X-AI-Suggestion-Id", "nope")])).is_none());
        let id = Uuid::new_v4().to_string();
        assert!(suggestion_id(&headers(&[("X-AI-Suggestion-Id", &id)])).is_some());
    }
}
