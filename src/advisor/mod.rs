//! The Apex advisor: a Claude agent over the fleet's data.
//!
//! It reads — FalconGo's Postgres (read-only transactions), etit-proxy's GPS
//! history and trip audit, maint-rust's due board, and the service-history
//! vectors — and it *suggests*. It never writes to another system's database:
//! a suggestion becomes a change only when an admin (permission 4) approves it,
//! and the change is then made through that system's own HTTP API with the
//! approver's token and `X-Via-AI: advisor`, so FalconGo records both the human
//! and the AI chat (the owner's rule, 2026-10-24).
//!
//! The module is self-contained (its own config, schema `advisor`, routes under
//! `/api/v1/advisor`) so it can move to its own service later without surgery.

pub mod actions;
pub mod agent;
pub mod anthropic;
pub mod digest;
pub mod handlers;
pub mod llm;
pub mod prompt;
pub mod search;
pub mod sql;
pub mod store;
pub mod tools;
pub mod upstream;

use std::sync::Arc;

use llm::LlmProvider;

/// Where the advisor's neighbours live. All default to the VPS's loopback
/// addresses, so production needs no extra env beyond the API key.
#[derive(Debug, Clone)]
pub struct AdvisorConfig {
    /// FalconGo's API root, including `/api`.
    pub falcon_api_url: String,
    /// etit-proxy root (its routes are under `/api/v1`).
    pub etit_proxy_url: String,
    /// etit-proxy's `X-Admin-Token`, for OSRM routing. Optional: without it
    /// route estimates fall back to straight-line distance.
    pub etit_admin_token: Option<String>,
    /// maint-rust root, including `/api/maint`.
    pub maint_api_url: String,
    /// The text-embeddings-inference endpoint FalconGo embeds invoices with.
    pub tei_url: String,
    /// Off by default in tests and on machines without the key.
    pub digest_enabled: bool,
}

impl AdvisorConfig {
    pub fn from_env() -> Self {
        Self {
            falcon_api_url: env_or("ADVISOR_FALCON_API_URL", "http://127.0.0.1:3001/api"),
            etit_proxy_url: env_or("ADVISOR_ETIT_PROXY_URL", "http://127.0.0.1:3003"),
            etit_admin_token: std::env::var("ETIT_PROXY_ADMIN_TOKEN")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            maint_api_url: env_or("ADVISOR_MAINT_API_URL", "http://127.0.0.1:8095/api/maint"),
            tei_url: env_or("ADVISOR_TEI_URL", "http://127.0.0.1:8080/embed"),
            digest_enabled: env_or("ADVISOR_DIGEST", "on") != "off",
        }
    }
}

/// Shared state for the advisor routes and the digest job.
#[derive(Clone)]
pub struct Advisor {
    /// `None` when `ANTHROPIC_API_KEY` is unset: the routes then answer 503
    /// and plain search falls back to the user's own words.
    pub provider: Option<Arc<dyn LlmProvider>>,
    pub http: reqwest::Client,
    pub cfg: AdvisorConfig,
}

impl Advisor {
    pub fn from_env() -> Self {
        let provider = anthropic::AnthropicProvider::from_env()
            .map(|p| Arc::new(p) as Arc<dyn LlmProvider>);
        match &provider {
            Some(p) => log::info!("advisor enabled ({})", p.name()),
            None => log::warn!("advisor disabled: ANTHROPIC_API_KEY is not set"),
        }
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("reqwest client");
        Self {
            provider,
            http,
            cfg: AdvisorConfig::from_env(),
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}
