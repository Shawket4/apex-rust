//! Calls to the advisor's neighbours. Every one carries the *caller's* token:
//! the advisor has no identity of its own, so it can never see or do more than
//! the admin talking to it.

use serde_json::{json, Value};

use super::AdvisorConfig;

/// The caller's FalconGo JWT, exactly as it arrived.
#[derive(Debug, Clone)]
pub struct BearerToken(pub String);

#[derive(Debug)]
pub struct UpstreamError(pub String);

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

async fn read_json(resp: reqwest::Response, what: &str) -> Result<Value, UpstreamError> {
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| UpstreamError(format!("{what}: {e}")))?;
    let body: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));
    if !status.is_success() {
        return Err(UpstreamError(format!("{what} answered {status}: {body}")));
    }
    Ok(body)
}

/// GET from etit-proxy's `/api/v1/…`.
pub async fn etit_get(
    http: &reqwest::Client,
    cfg: &AdvisorConfig,
    token: &BearerToken,
    path: &str,
    query: &[(&str, String)],
) -> Result<Value, UpstreamError> {
    let resp = http
        .get(format!("{}/api/v1{}", cfg.etit_proxy_url, path))
        .bearer_auth(&token.0)
        .query(query)
        .send()
        .await
        .map_err(|e| UpstreamError(format!("etit-proxy unreachable: {e}")))?;
    read_json(resp, "etit-proxy").await
}

/// Road distance and time between two points through etit-proxy's OSRM.
/// `None` when the admin token is not configured or routing failed — the
/// caller falls back to straight-line distance.
pub async fn osrm_route(
    http: &reqwest::Client,
    cfg: &AdvisorConfig,
    from: (f64, f64),
    to: (f64, f64),
) -> Option<(f64, f64)> {
    let token = cfg.etit_admin_token.as_ref()?;
    let resp = http
        .post(format!("{}/admin/routing/route", cfg.etit_proxy_url))
        .header("X-Admin-Token", token)
        .json(&json!({
            "from_lat": from.0, "from_lng": from.1,
            "to_lat": to.0, "to_lng": to.1,
            "restricted": true,
        }))
        .send()
        .await
        .ok()?;
    let v = read_json(resp, "osrm").await.ok()?;
    Some((v.get("distanceKm")?.as_f64()?, v.get("durationMin")?.as_f64()?))
}

/// GET from maint-rust (`/api/maint/…`).
pub async fn maint_get(
    http: &reqwest::Client,
    cfg: &AdvisorConfig,
    token: &BearerToken,
    path: &str,
    query: &[(&str, String)],
) -> Result<Value, UpstreamError> {
    let resp = http
        .get(format!("{}{}", cfg.maint_api_url, path))
        .bearer_auth(&token.0)
        .query(query)
        .send()
        .await
        .map_err(|e| UpstreamError(format!("maint-rust unreachable: {e}")))?;
    read_json(resp, "maint-rust").await
}

/// Which system an approved suggestion is applied through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Falcon,
    Maint,
}

/// Is FalconGo's AI audit log live? Checked before every apply: without it
/// FalconGo would accept the change but drop the `X-Via-AI` flag, breaking
/// the owner's rule that every AI change records its approver and the chat.
pub async fn falcon_audit_ready(
    http: &reqwest::Client,
    cfg: &AdvisorConfig,
    token: &BearerToken,
) -> Result<(), String> {
    let resp = http
        .get(format!("{}/ai-audit", cfg.falcon_api_url))
        .bearer_auth(&token.0)
        .query(&[("limit", "1")])
        .send()
        .await
        .map_err(|e| format!("FalconGo unreachable: {e}"))?;
    match resp.status().as_u16() {
        200..=299 => Ok(()),
        404 => Err("FalconGo's AI audit log is not deployed yet, so the change cannot be recorded as \
                    coming from the AI chat; the suggestion stays pending"
            .into()),
        code => Err(format!("FalconGo's AI audit check answered {code}; the suggestion stays pending")),
    }
}

/// One approved change, ready to send.
pub struct ApplyCall<'a> {
    pub target: Target,
    pub method: &'a str,
    pub path: &'a str,
    pub body: &'a Value,
    pub suggestion_id: uuid::Uuid,
}

/// Apply an approved suggestion: the approver's token plus the two headers
/// FalconGo's audit log keys on. Returns (HTTP status, body).
pub async fn apply(
    http: &reqwest::Client,
    cfg: &AdvisorConfig,
    token: &BearerToken,
    call: ApplyCall<'_>,
) -> Result<(u16, Value), UpstreamError> {
    let base = match call.target {
        Target::Falcon => &cfg.falcon_api_url,
        Target::Maint => &cfg.maint_api_url,
    };
    let method = reqwest::Method::from_bytes(call.method.as_bytes())
        .map_err(|_| UpstreamError(format!("bad method {}", call.method)))?;
    let resp = http
        .request(method, format!("{base}{}", call.path))
        .bearer_auth(&token.0)
        .header("X-Via-AI", "advisor")
        .header("X-AI-Suggestion-Id", call.suggestion_id.to_string())
        .json(call.body)
        .send()
        .await
        .map_err(|e| UpstreamError(format!("{:?} unreachable: {e}", call.target)))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let body = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));
    Ok((status, body))
}

/// Embed texts with the same TEI model FalconGo wrote the vectors with.
pub async fn embed(
    http: &reqwest::Client,
    cfg: &AdvisorConfig,
    texts: &[String],
) -> Result<Vec<Vec<f32>>, UpstreamError> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let resp = http
        .post(&cfg.tei_url)
        .json(&json!({ "inputs": texts }))
        .send()
        .await
        .map_err(|e| UpstreamError(format!("embedding service unreachable: {e}")))?;
    let v = read_json(resp, "embedding service").await?;
    serde_json::from_value(v).map_err(|e| UpstreamError(format!("embedding response: {e}")))
}
