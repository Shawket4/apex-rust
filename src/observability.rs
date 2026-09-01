//! Sentry wiring.
//!
//! Two rules govern this module.
//!
//! **It is off unless `SENTRY_DSN` is set.** With the variable unset or blank,
//! `init` returns `None`, no client is installed, and the service behaves
//! exactly as it did before Sentry existed. Nothing else in the codebase may
//! assume a client is present.
//!
//! **The scrubbing below is a compliance control, not a preference.** The
//! operator publishes a privacy policy stating that error reports exclude
//! personal data, and this is what makes that true. Apex carries driver names,
//! phone numbers, vehicle plates and GPS coordinates, all of which reach this
//! process routinely. Do not relax `before_send`, and do not set
//! `send_default_pii` to true, without a corresponding change to that policy.

use regex::Regex;
use std::borrow::Cow;

use sentry::protocol::{Context, Event, Value};
use sentry::ClientInitGuard;

/// Substrings that mark a key as carrying personal data. Matched against the
/// key with case and separators removed, so `driver_name`, `driverName` and
/// `DRIVER NAME` all match `name`.
///
/// These are deliberately broad. Over-redacting an error report costs a little
/// debugging context; under-redacting one puts a driver's phone number in a
/// database we promised it would not be in.
const REDACT_FRAGMENTS: &[&str] = &[
    "phone",
    "mobile",
    "email",
    "name",
    "address",
    "password",
    "passwd",
    "secret",
    "token",
    "authorization",
    "cookie",
    "session",
    "latitude",
    "longitude",
    "national",
    "ssn",
    // Payload-bearing field names. An upstream body is not ours to export and
    // cannot be cleaned once it is a string, so the value is dropped wherever
    // one of these names appears.
    "body",
    "payload",
    "response_text",
    "html",
    // Value-of-a-payload key names. "text" is what a messaging API calls the
    // thing a person wrote. Not "message": Sentry's own breadcrumbs use that
    // field and redacting it would blank the trail.
    "text",
    "caption",
    "content",
];

/// Keys that are personal in full but too short to match as substrings —
/// `lat` would otherwise match `plate` and `translate`.
const REDACT_EXACT: &[&str] = &[
    "lat", "lng", "lon", "long", "coords", "gps",
    // Short credential names the long ones miss: the list had "password" and
    // "passwd" but not "pass", which is what a login query string uses.
    "pass", "pwd", "user", "auth", "apikey", "sig", "jwt", "otp", "pin", "creds",
    // Keys whose VALUE is a person's name. "driver_name" is caught by the
    // "name" fragment; a bare "driver" is not. Exact, so "driver_id" survives.
    "driver", "employee", "owner", "contact", "person", "customer",
];

const REDACTED: &str = "[redacted]";

fn normalise(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Keys that match a denylist entry but are not personal.
///
/// `name` has to stay a fragment — it is what catches driver_name, first_name
/// and the rest — but it also matches the SDK's own metadata and the tags this
/// module sets itself. APEX-7 arrived from the Go service with job.name,
/// os.name and runtime.name all redacted, so the event could not say which job
/// had failed.
///
/// Exact matches only, and only for keys whose value is a machine's word for
/// itself. `device.name` is deliberately absent — that one is "Shawket's
/// iPhone".
const ALLOW_EXACT: &[&str] = &[
    "jobname", "osname", "runtimename", "browsername", "sdkname",
    "servicename", "rustname", "gonamespace",
];

fn is_sensitive(key: &str) -> bool {
    let k = normalise(key);
    if ALLOW_EXACT.contains(&k.as_str()) {
        return false;
    }
    REDACT_EXACT.contains(&k.as_str()) || REDACT_FRAGMENTS.iter().any(|f| k.contains(f))
}

/// Walk a JSON value, replacing the values of sensitive keys.
///
/// Recursive on purpose: the interesting data is never at the top level. A
/// trip payload nests the driver inside the trip inside the response, and a
/// flat pass over the outermost map would miss every one of them.
fn redact(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if is_sensitive(k) {
                    *v = Value::String(REDACTED.into());
                } else {
                    redact(v);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact),
        _ => {}
    }
}

/// Redact `k=v` pairs in a query string, keeping the keys so a URL still says
/// which filters were in play.
fn redact_query(query: &str) -> String {
    query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, _)) if is_sensitive(k) => format!("{k}={REDACTED}"),
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// The compliance control. See the module docs before changing anything here.
fn scrub(mut event: Event<'static>) -> Option<Event<'static>> {
    // Which service this came from. Every backend shares one Sentry project, so
    // without this you are left inferring it from the release string or the SDK
    // name. Set in before_send rather than on a scope so it is on every event,
    // whichever hub raised it. CARGO_PKG_NAME keeps it honest per crate.
    event
        .tags
        .insert("service".to_string(), env!("CARGO_PKG_NAME").to_string());

    // Whatever the SDK managed to infer about the person making the request,
    // drop it. `send_default_pii: false` already suppresses most of this;
    // clearing it outright means a future SDK release cannot quietly widen
    // what "default" covers.
    event.user = None;
    event.server_name = None;

    if let Some(request) = event.request.as_mut() {
        // The body is the highest-risk field in the whole event: on this
        // service it is trip and driver JSON.
        request.data = None;
        request.cookies = None;
        request.headers.retain(|name, _| !is_sensitive(name));
        if let Some(q) = request.query_string.as_ref() {
            request.query_string = Some(redact_query(q));
        }
        // `env` is CGI-style metadata (REMOTE_ADDR and friends), so string
        // values only — there is nothing to recurse into.
        for (k, v) in request.env.iter_mut() {
            if is_sensitive(k) {
                *v = REDACTED.to_string();
            }
        }
    }

    for (k, v) in event.extra.iter_mut() {
        if is_sensitive(k) {
            *v = Value::String(REDACTED.into());
        } else {
            redact(v);
        }
    }

    for (k, v) in event.tags.iter_mut() {
        if is_sensitive(k) {
            *v = REDACTED.to_string();
        }
    }

    for context in event.contexts.values_mut() {
        if let Context::Other(map) = context {
            for (k, v) in map.iter_mut() {
                if is_sensitive(k) {
                    *v = Value::String(REDACTED.into());
                } else {
                    redact(v);
                }
            }
        }
    }

    for crumb in event.breadcrumbs.iter_mut() {
        for (k, v) in crumb.data.iter_mut() {
            if is_sensitive(k) {
                *v = Value::String(REDACTED.into());
            } else {
                redact(v);
            }
        }
    }

    Some(event)
}

/// Fraction of requests traced.
///
/// Env-tunable so the rate can be dropped without a rebuild: this Sentry runs
/// on modest hardware and tracing is the part that loads it. Defaults to 0.5.
/// Anything unparseable or out of range falls back to the default rather than
/// panicking a service at boot over a typo.
fn traces_sample_rate() -> f32 {
    std::env::var("SENTRY_TRACES_SAMPLE_RATE")
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|r| (0.0..=1.0).contains(r))
        .unwrap_or(0.5)
}

/// Report a failed call to a third-party service.
///
/// Upstreams throttle, time out and change shape without warning, and those
/// failures are invisible to the HTTP middleware because this service usually
/// handles them and returns something sensible. They are exactly what you want
/// to see trending in Sentry.
///
/// Only the shape of the failure is attached — service, operation, status —
/// never the response body, which is where an upstream's own PII would be.
pub fn capture_upstream_failure(
    service: &str,
    operation: &str,
    status: Option<u16>,
    error: &dyn std::error::Error,
) {
    sentry::with_scope(
        |scope| {
            scope.set_tag("upstream.service", service);
            scope.set_tag("upstream.operation", operation);
            if let Some(code) = status {
                scope.set_tag("upstream.status", code);
            }
            // Throttling is its own failure mode and worth grouping apart.
            if status == Some(429) {
                scope.set_tag("upstream.throttled", "true");
            }
        },
        || {
            sentry::capture_error(error);
        },
    );
}

/// Report an upstream failure that has no error safe to attach.
///
/// Some upstream error types embed the response body in their `Display` — and
/// an upstream's body is exactly where its own users' data lives. The scrubber
/// redacts by KEY and so cannot clean a free-text message, which means such an
/// error must never be handed to `capture_upstream_failure`. This builds the
/// message from the shape alone instead.
pub fn capture_upstream_status(service: &str, operation: &str, status: u16) {
    sentry::with_scope(
        |scope| {
            scope.set_tag("upstream.service", service);
            scope.set_tag("upstream.operation", operation);
            scope.set_tag("upstream.status", status);
            if status == 429 {
                scope.set_tag("upstream.throttled", "true");
            }
        },
        || {
            sentry::capture_message(
                &format!("{service} {operation} returned {status}"),
                sentry::Level::Error,
            );
        },
    );
}

/// Make a free-text error message safe to export.
///
/// The scrubber redacts by KEY, walking structured data. It cannot help with a
/// message, which is why error text used to be withheld from Sentry entirely —
/// safe, but it left events thin: an operation name and no indication of what
/// actually went wrong.
///
/// This redacts inside the text instead, so the message can be attached in
/// full. It targets the shapes personal data actually takes in an error
/// string: a key=value pair in a URL query, a JSON field, a `key: value`
/// fragment, an email address, a phone number, a bearer token. The key
/// judgement is `is_sensitive`, the same one the scrubber uses, so the two
/// cannot drift.
///
/// Compliance control. Deliberately conservative: over-redacting costs a
/// little detail, under-redacting exports somebody's phone number.
pub fn sanitize_message(s: &str) -> String {
    use std::sync::OnceLock;

    static QUERY_PAIR: OnceLock<Regex> = OnceLock::new();
    static JSON_PAIR: OnceLock<Regex> = OnceLock::new();
    static COLON_PAIR: OnceLock<Regex> = OnceLock::new();
    static EMAIL: OnceLock<Regex> = OnceLock::new();
    static JWT: OnceLock<Regex> = OnceLock::new();
    static INTL_PHONE: OnceLock<Regex> = OnceLock::new();
    static LOCAL_MOBILE: OnceLock<Regex> = OnceLock::new();

    let query = QUERY_PAIR
        .get_or_init(|| Regex::new(r"([A-Za-z_][\w\-.]*)=([^&\s\x22'<>]+)").unwrap());
    let json = JSON_PAIR
        .get_or_init(|| Regex::new(r#""([^"]{1,64})"\s*:\s*"([^"]*)""#).unwrap());
    let colon = COLON_PAIR
        .get_or_init(|| Regex::new(r"\b([A-Za-z_][\w\-.]*)\s*:\s+([^\s,;}]+)").unwrap());
    let email = EMAIL.get_or_init(|| Regex::new(r"[\w.+\-]+@[\w\-]+\.[\w.\-]+").unwrap());
    let jwt = JWT.get_or_init(|| Regex::new(r"eyJ[\w\-]{4,}\.[\w\-]{4,}(\.[\w\-]+)?").unwrap());
    let intl = INTL_PHONE.get_or_init(|| Regex::new(r"\+\d[\d\s\-]{8,16}\d").unwrap());
    let local = LOCAL_MOBILE.get_or_init(|| Regex::new(r"\b01\d{9}\b").unwrap());

    // Keep the key, replace the value: the message still says WHICH field was
    // involved, which is usually the useful half.
    let redact_pair = |caps: &regex::Captures| -> String {
        let whole = caps.get(0).map(|m| m.as_str()).unwrap_or("");
        let key = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        if !is_sensitive(key) {
            return whole.to_string();
        }
        match whole.find([':', '=']) {
            Some(i) => format!("{}{}", &whole[..=i], REDACTED),
            None => whole.to_string(),
        }
    };

    let out = query.replace_all(s, &redact_pair).into_owned();
    let out = json.replace_all(&out, &redact_pair).into_owned();
    let out = colon.replace_all(&out, &redact_pair).into_owned();
    let out = email.replace_all(&out, REDACTED).into_owned();
    let out = jwt.replace_all(&out, REDACTED).into_owned();
    let out = intl.replace_all(&out, REDACTED).into_owned();
    let mut out = local.replace_all(&out, REDACTED).into_owned();

    // A message long enough to be a pasted payload is not a message.
    const MAX_LEN: usize = 2000;
    if out.len() > MAX_LEN {
        let cut = (0..=MAX_LEN).rev().find(|i| out.is_char_boundary(*i)).unwrap_or(0);
        out.truncate(cut);
        out.push_str("…[truncated]");
    }
    out
}

/// Install Sentry, or don't.
///
/// Returns `None` when `SENTRY_DSN` is unset or blank — the service then runs
/// with no client at all. The returned guard must be held for the lifetime of
/// the process; dropping it flushes and shuts the transport down.
pub fn init() -> Option<ClientInitGuard> {
    let dsn = std::env::var("SENTRY_DSN").ok()?;
    if dsn.trim().is_empty() {
        return None;
    }

    // Default by build profile rather than assuming production, so a developer
    // who exports a DSN locally does not file their experiments under prod.
    let environment: Cow<'static, str> = std::env::var("SENTRY_ENVIRONMENT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(Cow::Owned)
        .unwrap_or(Cow::Borrowed(if cfg!(debug_assertions) {
            "development"
        } else {
            "production"
        }));

    let release: Option<Cow<'static, str>> = std::env::var("SENTRY_RELEASE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(Cow::Owned)
        .or_else(|| sentry::release_name!());

    // Builder rather than a struct literal: ClientOptions is #[non_exhaustive].
    let options = sentry::ClientOptions::new()
        .maybe_release(release)
        .environment(environment)
        // Compliance: never send the SDK's idea of "default" personal data.
        .send_default_pii(false)
        .traces_sample_rate(traces_sample_rate())
        .before_send(scrub);

    Some(sentry::init((dsn, options)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_keys_are_recognised_in_any_shape() {
        for key in [
            "phone", "driver_name", "driverName", "DRIVER NAME", "e-mail",
            "Authorization", "password", "latitude", "national_id", "lat", "GPS",
        ] {
            assert!(is_sensitive(key), "{key} should be redacted");
        }
    }

    #[test]
    fn ordinary_keys_survive() {
        // `plate` and `translate` contain "lat"; matching that as a substring
        // would redact half the schema.
        for key in [
            "plate", "car_no_plate", "translate", "trip_id", "status", "count",
            "distance", "receipt_no", "template",
        ] {
            assert!(!is_sensitive(key), "{key} should not be redacted");
        }
    }

    #[test]
    fn redaction_reaches_nested_values() {
        let mut v = serde_json::json!({
            "trip": {
                "id": 7,
                "driver": { "name": "Someone Real", "phone": "+201234567890" },
                "stops": [{ "latitude": 30.1, "longitude": 31.2, "label": "depot" }]
            }
        });
        redact(&mut v);

        assert_eq!(v["trip"]["id"], 7, "non-sensitive values are left alone");
        // A sensitive key takes its whole subtree with it, rather than being
        // walked into and cleaned field by field. Costs the ids underneath;
        // means a personal value under an innocent-looking key cannot survive
        // inside a container we already know is personal.
        assert_eq!(v["trip"]["driver"], REDACTED);
        assert_eq!(v["trip"]["stops"][0]["latitude"], REDACTED);
        assert_eq!(v["trip"]["stops"][0]["longitude"], REDACTED);
        assert_eq!(v["trip"]["stops"][0]["label"], "depot");
    }

    #[test]
    fn query_strings_keep_their_keys_but_lose_sensitive_values() {
        assert_eq!(
            redact_query("month=2025-05&phone=%2B20123&company=Watanya"),
            "month=2025-05&phone=[redacted]&company=Watanya"
        );
    }

    #[test]
    fn scrub_drops_the_request_body_and_cookies() {
        let mut event = Event::default();
        event.request = Some(sentry::protocol::Request {
            data: Some("{\"driver_name\":\"Someone Real\"}".into()),
            cookies: Some("session=abc".into()),
            query_string: Some("lat=30.1&month=2025-05".into()),
            ..Default::default()
        });
        event.request.as_mut().unwrap().headers.insert(
            "Authorization".into(),
            "Bearer real-token".into(),
        );
        event.request.as_mut().unwrap().headers.insert(
            "User-Agent".into(),
            "curl/8".into(),
        );

        let out = scrub(event).expect("event is kept, only scrubbed");
        let req = out.request.unwrap();
        assert!(req.data.is_none(), "the body must never be sent");
        assert!(req.cookies.is_none());
        assert!(!req.headers.contains_key("Authorization"));
        assert_eq!(req.headers.get("User-Agent").map(String::as_str), Some("curl/8"));
        assert_eq!(req.query_string.as_deref(), Some("lat=[redacted]&month=2025-05"));
    }
}
