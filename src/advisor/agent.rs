//! The tool loop: ask, run what the model calls, feed the results back, until
//! it answers in prose or runs out of steps.

use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Value};

use super::llm::{Completion, LlmProvider, Message, ProviderError, Turn};
use super::tools::{self, ToolCtx};

/// Enough for find → itinerary → revenue → options → propose with retries.
const MAX_STEPS: usize = 14;
const MAX_TOKENS: u32 = 4096;
/// A tool result larger than this is cut before it reaches the model; the
/// tools already condense, this is the backstop.
const MAX_RESULT_CHARS: usize = 40_000;

#[derive(Debug, Serialize)]
pub struct ToolTrace {
    pub name: String,
    pub ms: u128,
    pub ok: bool,
}

pub struct Outcome {
    pub answer: String,
    /// Everything this turn added to the transcript, in order (the user's
    /// message first), for the store.
    pub new_messages: Vec<Message>,
    pub trace: Vec<ToolTrace>,
}

fn cap(v: Value) -> Value {
    let s = v.to_string();
    if s.len() <= MAX_RESULT_CHARS {
        return v;
    }
    let cut: String = s.chars().take(MAX_RESULT_CHARS).collect();
    json!({ "truncated": true, "partial": cut, "hint": "narrow the query (dates, one truck, aggregate)" })
}

/// Run one user turn on top of `history`.
/// Thinking from turns stored by EARLIER requests is dropped before replay.
///
/// Claude binds each thinking block's signature to the exact system prompt it
/// was produced under, and the system prompt carries today's date and the
/// Cairo time, so a conversation continued a minute later (or after a deploy
/// changed the prompt) was refused with "Invalid `signature` in `thinking`
/// block … bound to a different conversation". Earlier turns are complete, so
/// their thinking is not needed; only the current request's own tool loop
/// must keep its blocks, and that loop runs under one system prompt.
fn forget_old_thinking(mut history: Vec<Message>) -> Vec<Message> {
    for m in &mut history {
        if let Message::Assistant { calls, .. } = m {
            for c in calls {
                c.signature = None;
            }
        }
    }
    history
}

pub async fn run(
    provider: &dyn LlmProvider,
    system: &str,
    history: Vec<Message>,
    user_text: &str,
    ctx: &ToolCtx<'_>,
) -> Result<Outcome, ProviderError> {
    let defs = tools::tool_defs();
    let start = history.len();
    let mut messages = forget_old_thinking(history);
    messages.push(Message::User(user_text.to_string()));
    let mut trace = Vec::new();

    for step in 0..MAX_STEPS {
        // On the last step, tell the model to answer with what it has. (The
        // tools stay declared: the transcript holds tool calls, which need them.)
        if step + 1 == MAX_STEPS {
            messages.push(Message::User(
                "Step limit reached: answer now from what you have, without more tools.".into(),
            ));
        }
        let turn = provider
            .complete(Completion {
                system,
                messages: &messages,
                tools: &defs,
                max_tokens: MAX_TOKENS,
                force_tool: false,
            })
            .await?;
        match turn {
            Turn::Text(answer) => {
                messages.push(Message::Assistant { text: Some(answer.clone()), calls: vec![] });
                return Ok(Outcome { answer, new_messages: messages.split_off(start), trace });
            }
            Turn::Calls(calls) => {
                messages.push(Message::Assistant { text: None, calls: calls.clone() });
                for call in calls {
                    if let Some(events) = &ctx.events {
                        let _ = events.send(json!({ "name": call.name, "args": call.args }));
                    }
                    let t = Instant::now();
                    let result = tools::run(ctx, &call.name, &call.args).await;
                    let ok = result.get("error").is_none();
                    trace.push(ToolTrace { name: call.name.clone(), ms: t.elapsed().as_millis(), ok });
                    messages.push(Message::ToolResult {
                        id: call.id,
                        name: call.name,
                        content: cap(result),
                    });
                }
            }
        }
    }
    // The model kept calling tools past the nudge.
    let answer = "I could not finish this one in the steps I have — try a narrower question.".to_string();
    messages.push(Message::Assistant { text: Some(answer.clone()), calls: vec![] });
    Ok(Outcome { answer, new_messages: messages.split_off(start), trace })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_results_are_cut_with_a_hint() {
        let big = json!({ "rows": "x".repeat(MAX_RESULT_CHARS * 2) });
        let c = cap(big);
        assert_eq!(c["truncated"], true);
        assert!(c["partial"].as_str().unwrap().len() <= MAX_RESULT_CHARS);
        let small = json!({ "rows": [1, 2] });
        assert_eq!(cap(small.clone()), small);
    }

    #[test]
    fn stored_turns_replay_without_their_thinking() {
        use super::super::llm::ToolCall;
        let history = vec![
            Message::User("which truck earned most".into()),
            Message::Assistant {
                text: None,
                calls: vec![ToolCall {
                    id: "a".into(),
                    name: "fleet_overview".into(),
                    args: json!({}),
                    signature: Some(r#"[{"type":"thinking","signature":"EqQB"}]"#.into()),
                }],
            },
        ];
        let out = forget_old_thinking(history);
        match &out[1] {
            Message::Assistant { calls, .. } => {
                assert_eq!(calls[0].name, "fleet_overview");
                assert!(calls[0].signature.is_none());
            }
            other => panic!("{other:?}"),
        }
    }
}
