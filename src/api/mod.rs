//! REST API for bank-SMS transactions.
//!
//! Mounted under `/api/v1` alongside the existing apex-rust routes, except the
//! health probes which sit at the root by convention.
//!
//! Two rules apply everywhere:
//!   * Handlers take `AuthContext`, never raw `Claims`.
//!   * Reads return EFFECTIVE values -- `COALESCE(override, parsed_*)` -- plus a
//!     `parsed` sub-object holding the original, so the UI can show "the SMS said
//!     X, you changed it to Y".

pub mod admin;
pub mod health;
pub mod notes_tags;
pub mod raw;
pub mod transactions;

use actix_web::{web, HttpRequest, HttpMessage};

use crate::auth::AuthContext;
use crate::errors::{AppError, AppResult};

/// Pull the resolved authorization context off the request.
///
/// Absent means the token was valid but not an admin user (a driver token), which
/// is a 403 rather than a 401 -- the caller authenticated fine, they just have no
/// business here.
pub fn auth(req: &HttpRequest) -> AppResult<AuthContext> {
    req.extensions()
        .get::<AuthContext>()
        .cloned()
        .ok_or(AppError::Forbidden)
}

pub fn require_read(req: &HttpRequest) -> AppResult<AuthContext> {
    let ctx = auth(req)?;
    if !ctx.can_read() {
        return Err(AppError::Forbidden);
    }
    Ok(ctx)
}

pub fn require_write(req: &HttpRequest) -> AppResult<AuthContext> {
    let ctx = auth(req)?;
    if !ctx.can_write() {
        return Err(AppError::Forbidden);
    }
    Ok(ctx)
}

pub fn require_admin(req: &HttpRequest) -> AppResult<AuthContext> {
    let ctx = auth(req)?;
    if !ctx.can_admin() {
        return Err(AppError::Forbidden);
    }
    Ok(ctx)
}

/// Parse the `If-Match` header into a version number.
///
/// Mandatory on PUT/PATCH: without it two concurrent editors silently overwrite
/// each other. A missing header is 428, distinct from a malformed one (400), so
/// a client can tell "you forgot it" from "yours is wrong".
pub fn if_match_version(req: &HttpRequest) -> AppResult<i32> {
    let raw = req
        .headers()
        .get("If-Match")
        .ok_or(AppError::PreconditionRequired)?
        .to_str()
        .map_err(|_| AppError::BadRequest("If-Match is not valid ASCII".into()))?;

    // Accept both `3` and the quoted ETag form `"3"`.
    raw.trim()
        .trim_matches('"')
        .parse::<i32>()
        .map_err(|_| AppError::BadRequest(format!("If-Match must be a version number, got {raw:?}")))
}

/// Deserialize a date-or-datetime query parameter.
///
/// The expenses view puts its filters in the URL so a view can be shared, which
/// means people hand-edit them. `?from=2025-11-01` is the natural thing to type,
/// but a plain `DateTime<Utc>` field rejects it and the caller gets a 400 and an
/// empty page with no clue why.
///
/// Accepts:
///   * full RFC 3339 (`2025-11-01T00:00:00Z`) — what the date picker sends
///   * bare `YYYY-MM-DD`, anchored in **Africa/Cairo**, not UTC. Anchoring in
///     UTC would pull the previous evening's Cairo transactions into the range.
///
/// `end_of_day` decides which edge a bare date snaps to, so `to=2025-11-01`
/// includes that whole day rather than stopping at midnight.
pub mod flexible_date {
    use chrono::{DateTime, NaiveDate, NaiveTime, TimeZone, Utc};
    use chrono_tz::Africa::Cairo;
    use serde::{Deserialize, Deserializer};

    fn parse(raw: &str, end_of_day: bool) -> Option<DateTime<Utc>> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }

        if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
            return Some(dt.with_timezone(&Utc));
        }

        let date = NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()?;
        let time = if end_of_day {
            NaiveTime::from_hms_milli_opt(23, 59, 59, 999)?
        } else {
            NaiveTime::from_hms_opt(0, 0, 0)?
        };

        Cairo
            .from_local_datetime(&date.and_time(time))
            .earliest()
            .map(|dt| dt.with_timezone(&Utc))
    }

    pub fn start<'de, D>(deserializer: D) -> Result<Option<DateTime<Utc>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Option::<String>::deserialize(deserializer)?;
        Ok(raw.and_then(|s| parse(&s, false)))
    }

    pub fn end<'de, D>(deserializer: D) -> Result<Option<DateTime<Utc>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Option::<String>::deserialize(deserializer)?;
        Ok(raw.and_then(|s| parse(&s, true)))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn accepts_full_rfc3339() {
            let dt = parse("2025-11-01T08:30:00Z", false).unwrap();
            assert_eq!(dt.to_rfc3339(), "2025-11-01T08:30:00+00:00");
        }

        #[test]
        fn bare_date_anchors_to_cairo_not_utc() {
            // Cairo is UTC+2 on 1 Nov 2025, so local midnight is 22:00 UTC the
            // day before. Anchoring in UTC would sweep in the previous evening.
            let dt = parse("2025-11-01", false).unwrap();
            assert_eq!(dt.to_rfc3339(), "2025-10-31T22:00:00+00:00");
        }

        #[test]
        fn bare_end_date_covers_the_whole_day() {
            let start = parse("2025-11-01", false).unwrap();
            let end = parse("2025-11-01", true).unwrap();
            assert!(end > start);
            assert!((end - start).num_hours() >= 23);
        }

        #[test]
        fn garbage_and_empty_become_none_rather_than_an_error() {
            // A filter that cannot be understood should widen the view, not fail
            // the whole request.
            assert!(parse("not-a-date", false).is_none());
            assert!(parse("", false).is_none());
        }
    }
}

/// Cap on page size, so a client cannot ask for the whole ledger in one request.
pub const MAX_LIMIT: i64 = 200;
pub const DEFAULT_LIMIT: i64 = 50;

pub fn clamp_limit(requested: Option<i64>) -> i64 {
    requested.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    transactions::configure(cfg);
    notes_tags::configure(cfg);
    raw::configure(cfg);
    admin::configure(cfg);
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::test::TestRequest;

    #[test]
    fn if_match_accepts_bare_and_quoted_versions() {
        let req = TestRequest::default().insert_header(("If-Match", "3")).to_http_request();
        assert_eq!(if_match_version(&req).unwrap(), 3);

        let req = TestRequest::default().insert_header(("If-Match", "\"7\"")).to_http_request();
        assert_eq!(if_match_version(&req).unwrap(), 7);
    }

    #[test]
    fn missing_if_match_is_precondition_required_not_bad_request() {
        let req = TestRequest::default().to_http_request();
        assert!(matches!(
            if_match_version(&req),
            Err(AppError::PreconditionRequired)
        ));
    }

    #[test]
    fn malformed_if_match_is_bad_request() {
        let req = TestRequest::default().insert_header(("If-Match", "abc")).to_http_request();
        assert!(matches!(if_match_version(&req), Err(AppError::BadRequest(_))));
    }

    #[test]
    fn limit_is_clamped_to_a_sane_range() {
        assert_eq!(clamp_limit(None), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(Some(10_000)), MAX_LIMIT);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(-5)), 1);
        assert_eq!(clamp_limit(Some(25)), 25);
    }
}
