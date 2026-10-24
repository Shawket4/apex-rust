//! Ported from MadarRust `src/ai/llm.rs` (v1.25.0); keep the two in step.
//!
//! The LLM transport boundary.
//!
//! This trait carries **no domain knowledge**. It moves messages and tool
//! definitions to a model and brings back either text or tool calls — nothing
//! about reports, metrics or restaurants appears here.
//!
//! That is deliberate. In the previous design the trait had domain methods
//! (`choose_report`, `summarize`), which meant every new capability cost one new
//! method times every provider, and the providers drifted. With a transport
//! boundary, a new capability is one entry in `super::tools` and every
//! provider gets it for free.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A tool the model may call, described as JSON Schema. The same declaration is
/// used verbatim by every provider — Gemini and OpenAI-style APIs both accept
/// plain JSON Schema — so behavior cannot diverge by wire format.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: String,
    pub parameters: Value,
}

/// A tool call the model emitted.
// Serde (not in Madar): the advisor stores whole transcripts and replays them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Correlation id. OpenAI-style APIs supply one; Gemini matches tool results
    /// by name, so one is synthesized there.
    pub id: String,
    pub name: String,
    pub args: Value,
    /// Opaque provider state that must be echoed back verbatim when this call is
    /// replayed in a later turn.
    ///
    /// Gemini 3 returns a `thoughtSignature` on every `functionCall` part and
    /// **rejects the next request** if the signature is missing when that part
    /// is sent back — the model's reasoning is stateless across requests, and
    /// the signature is how it recovers what it was thinking. Dropping it fails
    /// the whole turn with "Function call is missing a thought_signature".
    ///
    /// It is deliberately opaque: nothing here interprets it, and providers that
    /// have no such concept (the OpenAI-style ones) leave it `None`.
    pub signature: Option<String>,
}

/// One turn of the conversation as the transport sees it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", content = "c", rename_all = "snake_case")]
pub enum Message {
    User(String),
    /// What the model said and/or asked to call.
    Assistant {
        text: Option<String>,
        calls: Vec<ToolCall>,
    },
    /// The outcome of a tool call, fed back so the model can continue — this is
    /// what turns a rejected query into a correction rather than a dead end.
    ToolResult {
        id: String,
        name: String,
        content: Value,
    },
}

/// A completion request.
pub struct Completion<'a> {
    pub system: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolDef],
    pub max_tokens: u32,
    /// Require a tool call rather than prose. The agent always sets this: it
    /// makes every provider's output parse identically, because even "here is
    /// your answer" arrives as an `answer` tool call.
    pub force_tool: bool,
}

/// What came back.
#[derive(Debug, Clone)]
pub enum Turn {
    Text(String),
    Calls(Vec<ToolCall>),
}

#[derive(Debug)]
pub enum ProviderError {
    /// No API key, or no configured backend. Distinct so the endpoint can report
    /// the feature as unavailable rather than as a bad request.
    NotConfigured(String),
    /// Network, HTTP status, or quota.
    Upstream(String),
    /// The response could not be parsed.
    Parse(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::NotConfigured(m) => write!(f, "{m}"),
            ProviderError::Upstream(m) => write!(f, "AI provider error: {m}"),
            ProviderError::Parse(m) => {
                write!(f, "AI provider returned an unexpected response: {m}")
            }
        }
    }
}

impl From<ProviderError> for crate::errors::AppError {
    fn from(e: ProviderError) -> Self {
        use crate::errors::AppError;
        match e {
            ProviderError::NotConfigured(m) => AppError::Upstream(m),
            ProviderError::Upstream(m) => AppError::Upstream(m),
            ProviderError::Parse(m) => AppError::Upstream(m),
        }
    }
}

/// A model backend. Transport only.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn complete(&self, req: Completion<'_>) -> Result<Turn, ProviderError>;
    /// Label for logs and responses, e.g. "gemini-3.1-flash-lite".
    fn name(&self) -> String;
}
