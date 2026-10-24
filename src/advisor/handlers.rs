//! `/api/v1/advisor/*` — every route permission 4 (the owner's rule).
//!
//! The chat answers as server-sent events: a turn can take a minute (several
//! tool calls), longer than a proxy will hold a silent request, so the stream
//! reports each tool as it runs and pings in between.

use std::sync::Mutex;
use std::time::Duration;

use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::llm::Message;
use super::tools::ToolCtx;
use super::upstream::{self, BearerToken, Target};
use super::{agent, digest, prompt, search, store, Advisor};
use crate::auth::permissions::AuthContext;
use crate::auth::JwtAuth;
use crate::errors::{AppError, AppResult};

fn admin() -> JwtAuth {
    JwtAuth {
        required_permission: Some(4),
    }
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api/v1/advisor")
            .route("/status", web::get().to(status).wrap(admin()))
            .route("/chat", web::post().to(chat).wrap(admin()))
            .route("/conversations", web::get().to(list_conversations).wrap(admin()))
            .route("/conversations/{id}", web::get().to(get_conversation).wrap(admin()))
            .route("/conversations/{id}", web::delete().to(delete_conversation).wrap(admin()))
            .route("/suggestions", web::get().to(list_suggestions).wrap(admin()))
            .route("/suggestions/{id}/approve", web::post().to(approve).wrap(admin()))
            .route("/suggestions/{id}/reject", web::post().to(reject).wrap(admin()))
            .route("/search", web::post().to(search_history).wrap(admin()))
            .route("/digests", web::get().to(list_digests).wrap(admin()))
            .route("/digests/run", web::post().to(run_digest).wrap(admin())),
    );
}

/// The verified caller. JwtAuth(4) has already checked the level; this is the
/// second lock, so a route registered without the wrapper still fails closed.
fn caller(req: &HttpRequest) -> AppResult<AuthContext> {
    let ctx = req
        .extensions()
        .get::<AuthContext>()
        .cloned()
        .ok_or(AppError::Unauthenticated)?;
    if !ctx.has_at_least(4) {
        return Err(AppError::Forbidden);
    }
    Ok(ctx)
}

/// The caller's JWT as it arrived (cookie first, as JwtAuth reads it), to
/// forward to FalconGo, etit-proxy and maint-rust.
fn bearer(req: &HttpRequest) -> AppResult<BearerToken> {
    req.cookie("jwt")
        .map(|c| c.value().to_string())
        .or_else(|| {
            req.headers()
                .get("Authorization")
                .and_then(|h| h.to_str().ok())
                .and_then(|h| h.strip_prefix("Bearer "))
                .map(str::to_string)
        })
        .map(BearerToken)
        .ok_or(AppError::Unauthenticated)
}

fn provider(adv: &Advisor) -> AppResult<std::sync::Arc<dyn super::llm::LlmProvider>> {
    adv.provider
        .clone()
        .ok_or_else(|| AppError::BadRequest("the advisor is not configured (ANTHROPIC_API_KEY)".into()))
}

async fn status(adv: web::Data<Advisor>, req: HttpRequest) -> AppResult<HttpResponse> {
    caller(&req)?;
    Ok(HttpResponse::Ok().json(json!({
        "enabled": adv.provider.is_some(),
        "model": adv.provider.as_ref().map(|p| p.name()),
    })))
}

#[derive(Deserialize)]
struct ChatBody {
    conversation_id: Option<Uuid>,
    message: String,
}

fn sse(event: &str, data: &Value) -> web::Bytes {
    web::Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

async fn chat(
    pool: web::Data<PgPool>,
    adv: web::Data<Advisor>,
    req: HttpRequest,
    body: web::Json<ChatBody>,
) -> AppResult<HttpResponse> {
    let who = caller(&req)?;
    let token = bearer(&req)?;
    let provider = provider(&adv)?;
    let message = body.message.trim().to_string();
    if message.is_empty() || message.chars().count() > 4000 {
        return Err(AppError::BadRequest("message must be 1-4000 characters".into()));
    }
    let conversation_id = match body.conversation_id {
        Some(id) if store::owns(&pool, id, who.user_id).await? => id,
        Some(_) => return Err(AppError::NotFound("conversation".into())),
        None => store::create_conversation(&pool, who.user_id, &message).await?,
    };
    let history = store::replay_window(store::transcript(&pool, conversation_id).await?);

    let (tx, rx) = mpsc::channel::<web::Bytes>(32);
    let pool = pool.get_ref().clone();
    let adv = adv.get_ref().clone();
    tokio::spawn(async move {
        let _ = tx
            .send(sse("conversation", &json!({ "conversation_id": conversation_id })))
            .await;
        let (events_tx, mut events_rx) = mpsc::unbounded_channel::<Value>();
        let ctx = ToolCtx {
            pool: &pool,
            adv: &adv,
            token: &token,
            user_id: who.user_id,
            conversation_id: Some(conversation_id),
            proposed: Mutex::new(Vec::new()),
            events: Some(events_tx),
        };
        let system = prompt::system();
        let run = agent::run(provider.as_ref(), &system, history, &message, &ctx);
        tokio::pin!(run);
        let mut ping = tokio::time::interval(Duration::from_secs(15));
        let outcome = loop {
            tokio::select! {
                out = &mut run => break out,
                Some(ev) = events_rx.recv() => { let _ = tx.send(sse("tool", &ev)).await; }
                _ = ping.tick() => { let _ = tx.send(web::Bytes::from_static(b": ping\n\n")).await; }
            }
        };
        match outcome {
            Ok(out) => {
                if let Err(e) = store::append(&pool, conversation_id, &out.new_messages).await {
                    log::error!("advisor: could not store the turn: {e}");
                }
                let ids = ctx.proposed.lock().map(|v| v.clone()).unwrap_or_default();
                let mut suggestions = Vec::new();
                for id in ids {
                    if let Ok(Some(s)) = store::suggestion(&pool, id).await {
                        suggestions.push(s);
                    }
                }
                let _ = tx
                    .send(sse(
                        "answer",
                        &json!({
                            "conversation_id": conversation_id,
                            "answer": out.answer,
                            "suggestions": suggestions,
                            "tools": out.trace,
                        }),
                    ))
                    .await;
            }
            Err(e) => {
                log::warn!("advisor turn failed: {e}");
                // The user's message is kept so the conversation reads right.
                let _ = store::append(&pool, conversation_id, &[Message::User(message.clone())]).await;
                let _ = tx.send(sse("error", &json!({ "message": e.to_string() }))).await;
            }
        }
    });

    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|b| (Ok::<_, actix_web::Error>(b), rx))
    });
    Ok(HttpResponse::Ok()
        .content_type("text/event-stream")
        .insert_header(("Cache-Control", "no-cache"))
        // nginx buffers proxied responses unless told otherwise per response.
        .insert_header(("X-Accel-Buffering", "no"))
        .streaming(stream))
}

async fn list_conversations(pool: web::Data<PgPool>, req: HttpRequest) -> AppResult<HttpResponse> {
    let who = caller(&req)?;
    Ok(HttpResponse::Ok().json(store::list_conversations(&pool, who.user_id).await?))
}

/// The transcript as the user saw it: their messages and the answers, with
/// the tools each answer used. Tool payloads stay server-side.
fn render(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut tools: Vec<String> = Vec::new();
    for m in messages {
        match m {
            Message::User(t) if t.starts_with("Step limit reached:") => {}
            Message::User(t) => {
                tools.clear();
                out.push(json!({ "role": "user", "text": t }));
            }
            Message::Assistant { calls, text } => {
                tools.extend(calls.iter().map(|c| c.name.clone()));
                if let Some(t) = text.as_ref().filter(|t| !t.trim().is_empty()) {
                    out.push(json!({ "role": "assistant", "text": t, "tools": std::mem::take(&mut tools) }));
                }
            }
            Message::ToolResult { .. } => {}
        }
    }
    out
}

async fn get_conversation(
    pool: web::Data<PgPool>,
    req: HttpRequest,
    path: web::Path<Uuid>,
) -> AppResult<HttpResponse> {
    let who = caller(&req)?;
    let id = path.into_inner();
    if !store::owns(&pool, id, who.user_id).await? {
        return Err(AppError::NotFound("conversation".into()));
    }
    let messages = store::transcript(&pool, id).await?;
    let suggestions = store::suggestions(&pool, None, Some(id)).await?;
    Ok(HttpResponse::Ok().json(json!({
        "id": id,
        "messages": render(&messages),
        "suggestions": suggestions,
    })))
}

async fn delete_conversation(
    pool: web::Data<PgPool>,
    req: HttpRequest,
    path: web::Path<Uuid>,
) -> AppResult<HttpResponse> {
    let who = caller(&req)?;
    if store::delete_conversation(&pool, path.into_inner(), who.user_id).await? {
        Ok(HttpResponse::NoContent().finish())
    } else {
        Err(AppError::NotFound("conversation".into()))
    }
}

#[derive(Deserialize)]
struct SuggestionQuery {
    status: Option<String>,
    conversation_id: Option<Uuid>,
}

async fn list_suggestions(
    pool: web::Data<PgPool>,
    req: HttpRequest,
    q: web::Query<SuggestionQuery>,
) -> AppResult<HttpResponse> {
    caller(&req)?;
    Ok(HttpResponse::Ok().json(store::suggestions(&pool, q.status.as_deref(), q.conversation_id).await?))
}

/// Approve and apply: through the owning service's API, with the approver's
/// own token, marked `X-Via-AI: advisor` — FalconGo logs the human and the AI.
async fn approve(
    pool: web::Data<PgPool>,
    adv: web::Data<Advisor>,
    req: HttpRequest,
    path: web::Path<Uuid>,
) -> AppResult<HttpResponse> {
    let who = caller(&req)?;
    let token = bearer(&req)?;
    let id = path.into_inner();
    let s = match store::decide(&pool, id, who.user_id, "applying").await? {
        Some(s) => s,
        None => {
            return match store::suggestion(&pool, id).await? {
                Some(s) => Err(AppError::Conflict(format!("suggestion is already {}", s.status))),
                None => Err(AppError::NotFound("suggestion".into())),
            }
        }
    };
    let target = if s.target == "maint" { Target::Maint } else { Target::Falcon };
    let call = upstream::ApplyCall {
        target,
        method: &s.method,
        path: &s.path,
        body: &s.body,
        suggestion_id: s.id,
    };
    let applied = upstream::apply(&adv.http, &adv.cfg, &token, call).await;
    let row = match applied {
        Ok((code, body)) => {
            let ok = (200..300).contains(&code);
            store::record_result(&pool, s.id, ok, Some(code), &body).await?
        }
        Err(e) => store::record_result(&pool, s.id, false, None, &json!({ "error": e.to_string() })).await?,
    };
    Ok(HttpResponse::Ok().json(row))
}

async fn reject(
    pool: web::Data<PgPool>,
    req: HttpRequest,
    path: web::Path<Uuid>,
) -> AppResult<HttpResponse> {
    let who = caller(&req)?;
    let id = path.into_inner();
    match store::decide(&pool, id, who.user_id, "rejected").await? {
        Some(s) => Ok(HttpResponse::Ok().json(s)),
        None => match store::suggestion(&pool, id).await? {
            Some(s) => Err(AppError::Conflict(format!("suggestion is already {}", s.status))),
            None => Err(AppError::NotFound("suggestion".into())),
        },
    }
}

#[derive(Deserialize)]
struct SearchBody {
    query: String,
    car_id: Option<i64>,
    plate_number: Option<String>,
    since: Option<chrono::NaiveDate>,
    limit: Option<usize>,
}

/// The search box: Claude expands the words, then the hybrid search ranks.
async fn search_history(
    pool: web::Data<PgPool>,
    adv: web::Data<Advisor>,
    req: HttpRequest,
    body: web::Json<SearchBody>,
) -> AppResult<HttpResponse> {
    caller(&req)?;
    let query = body.query.trim();
    if query.is_empty() {
        return Err(AppError::BadRequest("query is required".into()));
    }
    let (variants, plate) = search::expand(adv.provider.as_deref(), query).await;
    let filters = search::Filters {
        car_id: body.car_id,
        plate: body.plate_number.clone().filter(|p| !p.trim().is_empty()).or(plate),
        since: body.since,
    };
    let limit = body.limit.unwrap_or(30).clamp(1, 100);
    let hits = search::hybrid(&pool, &adv.http, &adv.cfg, &variants, &filters, limit)
        .await
        .map_err(AppError::Internal)?;
    Ok(HttpResponse::Ok().json(json!({
        "query": query,
        "variants": variants,
        "plate_number": filters.plate,
        "count": hits.len(),
        "hits": hits,
    })))
}

async fn list_digests(pool: web::Data<PgPool>, req: HttpRequest) -> AppResult<HttpResponse> {
    caller(&req)?;
    Ok(HttpResponse::Ok().json(store::digests(&pool).await?))
}

#[derive(Deserialize)]
struct DigestBody {
    days: Option<i64>,
}

/// Write a digest now, in the background, with the caller's token for the
/// tools that need one. 202; it appears in GET /digests when done.
async fn run_digest(
    pool: web::Data<PgPool>,
    adv: web::Data<Advisor>,
    req: HttpRequest,
    body: Option<web::Json<DigestBody>>,
) -> AppResult<HttpResponse> {
    let who = caller(&req)?;
    let token = bearer(&req)?;
    provider(&adv)?;
    let days = body.and_then(|b| b.days).unwrap_or(7).clamp(1, 31);
    let pool = pool.get_ref().clone();
    let adv = adv.get_ref().clone();
    tokio::spawn(async move {
        if let Err(e) = digest::write(&pool, &adv, &token, who.user_id, days).await {
            log::warn!("advisor digest failed: {e}");
        }
    });
    Ok(HttpResponse::Accepted().json(json!({ "status": "started", "days": days })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisor::llm::ToolCall;

    #[test]
    fn the_rendered_transcript_hides_tool_payloads() {
        let msgs = vec![
            Message::User("where is 1234?".into()),
            Message::Assistant {
                text: None,
                calls: vec![ToolCall { id: "a".into(), name: "find_truck".into(), args: json!({}), signature: None }],
            },
            Message::ToolResult { id: "a".into(), name: "find_truck".into(), content: json!({ "secret": 1 }) },
            Message::User("Step limit reached: answer now".into()),
            Message::Assistant { text: Some("It is in Suez.".into()), calls: vec![] },
        ];
        let r = render(&msgs);
        assert_eq!(r.len(), 2);
        assert_eq!(r[1]["tools"], json!(["find_truck"]));
        assert!(!r.iter().any(|m| m.to_string().contains("secret")));
    }
}
