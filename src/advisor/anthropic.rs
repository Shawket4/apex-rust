//! Ported from MadarRust `src/ai/anthropic.rs` (v1.25.0); keep the two in step.
//!
//! Anthropic transport (Claude Messages API).
//!
//! Wire format and HTTP only. Select it with `AI_PROVIDER=anthropic` and
//! `ANTHROPIC_API_KEY`; `ANTHROPIC_MODEL` overrides the model (Claude Haiku 5.5
//! by default) and `ANTHROPIC_EFFORT` the effort level.
//!
//! Three properties of this API shape the translation:
//!
//!   * tool results are `tool_result` blocks on a **user** turn, and every
//!     result for one assistant turn must arrive in the single user turn right
//!     after it — so consecutive transport messages of the same role are folded
//!     into one wire message;
//!   * Claude Haiku 5.5 rejects any non-default `temperature`, so none is sent
//!     (unlike the other transports, which pin it to 0);
//!   * the newest larger models (Opus 5.5, Sonnet 5.5, Fable 5.1) reject a
//!     forced `tool_choice`. Haiku accepts it, so it is sent, and a model that
//!     refuses it is retried once with `auto` and remembered — the agent already
//!     takes a prose answer at its word, so nothing downstream changes.
//!
//! Cost: the tool declarations and system prompt are byte-stable and render
//! first, so a breakpoint on the system block caches both across every
//! merchant; the request-level breakpoint then caches the growing conversation
//! across the agent's steps.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::llm::{Completion, LlmProvider, Message, ProviderError, ToolCall, ToolDef, Turn};

const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";
/// Default model — override with `ANTHROPIC_MODEL`.
const DEFAULT_MODEL: &str = "claude-haiku-5-5";
/// Default effort — override with `ANTHROPIC_EFFORT` (`low` … `max`, or `none`
/// to omit the field for a model that has no effort levels, e.g. Haiku 4.5).
/// Low matches the other transports, which run with thinking off: a forced
/// tool call skips thinking anyway, so this mostly governs the prose-only
/// compaction call.
const DEFAULT_EFFORT: &str = "low";
// Longer than Madar's 30 s: the advisor's answers (a digest, a reassignment
// plan) run to a few thousand tokens.
const HTTP_TIMEOUT_SECS: u64 = 90;

pub struct AnthropicProvider {
    api_key: String,
    model: String,
    effort: Option<String>,
    http: reqwest::Client,
    /// Set once the model has rejected a forced `tool_choice`, so later
    /// requests go straight to `auto` instead of paying for the 400 each time.
    forced_tool_rejected: AtomicBool,
}

impl AnthropicProvider {
    /// Build from `ANTHROPIC_API_KEY` (+ optional `ANTHROPIC_MODEL`,
    /// `ANTHROPIC_EFFORT`). `None` when the key is unset.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").ok()?;
        if api_key.trim().is_empty() {
            return None;
        }
        let model = env_or("ANTHROPIC_MODEL", DEFAULT_MODEL);
        let effort = Some(env_or("ANTHROPIC_EFFORT", DEFAULT_EFFORT).to_ascii_lowercase())
            .filter(|e| e != "none");
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()
            .ok()?;
        Some(Self {
            api_key: api_key.trim().to_string(),
            model,
            effort,
            http,
            forced_tool_rejected: AtomicBool::new(false),
        })
    }

    fn body(&self, req: &Completion<'_>, force: bool) -> Value {
        let mut body = json!({
            "model": self.model,
            "max_tokens": req.max_tokens,
            "system": [{
                "type": "text",
                "text": req.system,
                "cache_control": { "type": "ephemeral" }
            }],
            "messages": wire_messages(req.messages),
            // Auto-placed on the last cacheable block: the agent's later steps
            // re-read everything before it at the cache rate.
            "cache_control": { "type": "ephemeral" },
        });
        if let Some(effort) = &self.effort {
            body["output_config"] = json!({ "effort": effort });
        }
        // Omitted entirely when there are none: the compaction call offers no
        // tools because it wants prose.
        if !req.tools.is_empty() {
            body["tools"] = json!(tool_specs(req.tools));
            body["tool_choice"] = json!({ "type": if force { "any" } else { "auto" } });
        }
        body
    }

    async fn post(&self, body: &Value) -> Result<Value, ProviderError> {
        let resp = self
            .http
            .post(ENDPOINT)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .json(body)
            .send()
            .await
            .map_err(|e| ProviderError::Upstream(e.to_string()))?;
        let status = resp.status();
        let payload: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Parse(e.to_string()))?;
        if !status.is_success() {
            let msg = payload
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("upstream error");
            return Err(ProviderError::Upstream(format!("{status}: {msg}")));
        }
        Ok(payload)
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn complete(&self, req: Completion<'_>) -> Result<Turn, ProviderError> {
        let force = req.force_tool && !self.forced_tool_rejected.load(Ordering::Relaxed);
        let payload = match self.post(&self.body(&req, force)).await {
            Err(ProviderError::Upstream(msg)) if force && rejects_forced_tool(&msg) => {
                tracing::info!(
                    "{} rejects a forced tool_choice; using auto from now on",
                    self.model
                );
                self.forced_tool_rejected.store(true, Ordering::Relaxed);
                self.post(&self.body(&req, false)).await?
            }
            other => other?,
        };
        parse_turn(&payload)
    }

    fn name(&self) -> String {
        format!("anthropic/{}", self.model)
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// The 400 a model that does not support forced tool use returns:
/// `tool_choice: type "tool" and "any" are not supported for this model.`
fn rejects_forced_tool(msg: &str) -> bool {
    msg.starts_with("400") && msg.contains("tool_choice")
}

fn parse_turn(payload: &Value) -> Result<Turn, ProviderError> {
    // A safety classifier declined the request: HTTP 200 with no usable
    // content. Reported as upstream so the merchant sees "unavailable" rather
    // than an empty answer.
    if payload.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
        return Err(ProviderError::Upstream(
            "the model declined the request".into(),
        ));
    }
    let blocks = payload
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::Parse("no content in the response".into()))?;

    // With thinking on (any non-forced turn), the thinking blocks that preceded
    // a tool call must be sent back unchanged when the call is replayed with
    // its result. They ride on the first call's opaque `signature`.
    let thinking: Vec<&Value> = blocks
        .iter()
        .filter(|b| {
            matches!(
                b.get("type").and_then(Value::as_str),
                Some("thinking" | "redacted_thinking")
            )
        })
        .collect();

    let mut calls: Vec<ToolCall> = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|b| {
            Some(ToolCall {
                id: b.get("id").and_then(Value::as_str)?.to_string(),
                name: b.get("name").and_then(Value::as_str)?.to_string(),
                args: b.get("input").cloned().unwrap_or_else(|| json!({})),
                signature: None,
            })
        })
        .collect();
    if let Some(first) = calls.first_mut() {
        if !thinking.is_empty() {
            first.signature = Some(json!(thinking).to_string());
        }
        return Ok(Turn::Calls(calls));
    }

    let text = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    if text.is_empty() {
        return Err(ProviderError::Parse(
            "no tool call and no text in the response".into(),
        ));
    }
    Ok(Turn::Text(text))
}

fn tool_specs(tools: &[ToolDef]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
            })
        })
        .collect()
}

/// Tool-use ids must match `^[a-zA-Z0-9_-]+$`. Ours come from this API, but a
/// call synthesized elsewhere is mapped rather than rejected.
fn wire_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Translate the transport's messages into Claude ones: content-block arrays,
/// with consecutive same-role turns folded together so tool results for one
/// assistant turn arrive in a single user turn.
fn wire_messages(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut push = |role: &str, blocks: Vec<Value>| {
        if blocks.is_empty() {
            return;
        }
        if let Some(last) = out.last_mut().filter(|m| m["role"] == role) {
            if let Some(arr) = last["content"].as_array_mut() {
                arr.extend(blocks);
            }
            return;
        }
        out.push(json!({ "role": role, "content": blocks }));
    };
    for m in messages {
        match m {
            Message::User(text) if text.trim().is_empty() => {}
            Message::User(text) => push("user", vec![json!({ "type": "text", "text": text })]),
            Message::Assistant { text, calls } => {
                let mut blocks: Vec<Value> = Vec::new();
                // Thinking blocks go first, exactly as they arrived.
                if let Some(sig) = calls.first().and_then(|c| c.signature.as_deref()) {
                    if let Ok(Value::Array(thinking)) = serde_json::from_str(sig) {
                        blocks.extend(thinking);
                    }
                }
                if let Some(t) = text.as_ref().filter(|t| !t.trim().is_empty()) {
                    blocks.push(json!({ "type": "text", "text": t }));
                }
                for c in calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": wire_id(&c.id),
                        "name": c.name,
                        "input": c.args,
                    }));
                }
                push("assistant", blocks);
            }
            Message::ToolResult { id, content, .. } => push(
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": wire_id(id),
                    "content": content.to_string(),
                })],
            ),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, signature: Option<&str>) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "run_preset".into(),
            args: json!({ "preset": "top_products" }),
            signature: signature.map(str::to_string),
        }
    }

    #[test]
    fn parallel_tool_results_fold_into_one_user_turn() {
        // The API requires every result for an assistant turn in the single
        // user turn right after it.
        let msgs = vec![
            Message::User("q".into()),
            Message::Assistant {
                text: None,
                calls: vec![call("toolu_1", None), call("toolu_2", None)],
            },
            Message::ToolResult {
                id: "toolu_1".into(),
                name: "run_preset".into(),
                content: json!({ "row_count": 5 }),
            },
            Message::ToolResult {
                id: "toolu_2".into(),
                name: "run_preset".into(),
                content: json!({ "row_count": 3 }),
            },
        ];
        let w = wire_messages(&msgs);
        assert_eq!(w.len(), 3);
        assert_eq!(w[1]["content"][1]["id"], "toolu_2");
        assert_eq!(w[2]["role"], "user");
        assert_eq!(w[2]["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(w[2]["content"][1]["tool_use_id"], "toolu_2");
    }

    #[test]
    fn consecutive_user_turns_merge_and_blank_turns_vanish() {
        // The agent opens with the grounding and the question as two user
        // turns; an assistant turn with nothing in it is a 400.
        let msgs = vec![
            Message::User("grounding".into()),
            Message::User("question".into()),
            Message::Assistant {
                text: Some("  ".into()),
                calls: vec![],
            },
        ];
        let w = wire_messages(&msgs);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn thinking_blocks_round_trip_ahead_of_the_call() {
        let payload = json!({
            "stop_reason": "tool_use",
            "content": [
                { "type": "thinking", "thinking": "", "signature": "EqQB..." },
                { "type": "tool_use", "id": "toolu_1", "name": "answer", "input": { "text": "hi" } }
            ]
        });
        let Turn::Calls(calls) = parse_turn(&payload).unwrap() else {
            panic!("expected calls");
        };
        assert_eq!(calls[0].args["text"], "hi");
        let w = wire_messages(&[Message::Assistant { text: None, calls }]);
        assert_eq!(w[0]["content"][0]["type"], "thinking");
        assert_eq!(w[0]["content"][0]["signature"], "EqQB...");
        assert_eq!(w[0]["content"][1]["type"], "tool_use");
    }

    #[test]
    fn a_refusal_is_an_upstream_error_not_an_empty_answer() {
        let payload = json!({ "stop_reason": "refusal", "content": [] });
        assert!(matches!(
            parse_turn(&payload),
            Err(ProviderError::Upstream(_))
        ));
    }

    #[test]
    fn text_blocks_join_when_there_is_no_call() {
        let payload = json!({
            "stop_reason": "end_turn",
            "content": [{ "type": "text", "text": "Sales rose 4%." }]
        });
        assert!(matches!(parse_turn(&payload), Ok(Turn::Text(t)) if t == "Sales rose 4%."));
    }

    #[test]
    fn ids_from_other_transports_are_made_wire_safe() {
        assert_eq!(wire_id("call.abc:1"), "call_abc_1");
        assert_eq!(wire_id("run_preset-0"), "run_preset-0");
    }

    #[test]
    fn only_the_forced_tool_choice_400_triggers_the_auto_retry() {
        assert!(rejects_forced_tool(
            "400 Bad Request: tool_choice: type \"tool\" and \"any\" are not supported for this model."
        ));
        assert!(!rejects_forced_tool("429 Too Many Requests: rate limited"));
    }

    #[test]
    fn tool_specs_pass_the_schema_through_unchanged() {
        let tools = [ToolDef {
            name: "run_sql",
            description: "d".into(),
            parameters: json!({ "type": "object", "properties": { "query": { "type": "string" } } }),
        }];
        let s = tool_specs(&tools);
        assert_eq!(s[0]["name"], tools[0].name);
        assert_eq!(s[0]["input_schema"], tools[0].parameters);
    }
}
