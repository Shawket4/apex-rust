//! The Messages screen's API: every verdict the parser made, searchable.
//!
//! This replaces the old review-queue endpoints. There is no queue — these
//! are verdicts, not work items. The one mutation that exists (recording an
//! ignored message as a transaction) lives on POST /transactions with
//! `raw_message_id`, so the human decision is a normal transaction row.

use actix_web::{web, HttpResponse};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, QueryBuilder, Row};

use crate::errors::{AppError, AppResult};

#[derive(Debug, Serialize)]
pub struct MessageView {
    pub id: i64,
    pub wa_message_id: String,
    pub wa_timestamp: DateTime<Utc>,
    pub body: String,
    pub media_type: Option<String>,
    pub is_from_me: bool,
    pub status: String,
    pub template: Option<String>,
    pub transaction_id: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct MessagePage {
    pub data: Vec<MessageView>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MessagesQuery {
    pub status: Option<String>,
    pub q: Option<String>,
    /// Media messages have empty bodies — 254 of them would drown the Ignored
    /// view, so they are hidden unless explicitly requested.
    #[serde(default)]
    pub include_media: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

const SELECT: &str = "SELECT r.id, r.wa_message_id, r.wa_timestamp, r.body, r.media_type, \
     r.is_from_me, r.status, r.template, t.id AS transaction_id \
     FROM banksms.raw_messages r \
     LEFT JOIN banksms.transactions t ON t.raw_message_id = r.id AND t.deleted_at IS NULL";

fn row_to_view(r: &sqlx::postgres::PgRow) -> MessageView {
    MessageView {
        id: r.get("id"),
        wa_message_id: r.get("wa_message_id"),
        wa_timestamp: r.get("wa_timestamp"),
        body: r.get("body"),
        media_type: r.get("media_type"),
        is_from_me: r.get("is_from_me"),
        status: r.get("status"),
        template: r.get("template"),
        transaction_id: r.get("transaction_id"),
    }
}

pub async fn list(
    pool: web::Data<PgPool>,
    query: web::Query<MessagesQuery>,
) -> AppResult<HttpResponse> {
    let f = query.into_inner();
    let limit = f.limit.unwrap_or(50).min(200) as i64;

    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(SELECT);
    qb.push(" WHERE TRUE");

    if let Some(status) = &f.status {
        if !["ignored", "suppressed", "matched"].contains(&status.as_str()) {
            return Err(AppError::BadRequest(format!("unknown status '{status}'")));
        }
        qb.push(" AND r.status = ").push_bind(status);
    }
    let include_media = matches!(f.include_media.as_deref(), Some("true") | Some("1"));
    if !include_media {
        qb.push(" AND r.body <> ''");
    }
    if let Some(q) = &f.q {
        qb.push(" AND r.body ILIKE '%' || ")
            .push_bind(q)
            .push(" || '%'");
    }
    if let Some(c) = &f.cursor {
        let (millis, id) = c
            .split_once(':')
            .ok_or_else(|| AppError::BadRequest("malformed cursor".into()))?;
        let millis: i64 = millis
            .parse()
            .map_err(|_| AppError::BadRequest("malformed cursor".into()))?;
        let id: i64 = id
            .parse()
            .map_err(|_| AppError::BadRequest("malformed cursor".into()))?;
        let ts = DateTime::<Utc>::from_timestamp_millis(millis)
            .ok_or_else(|| AppError::BadRequest("malformed cursor".into()))?;
        qb.push(" AND (r.wa_timestamp, r.id) < (")
            .push_bind(ts)
            .push(", ")
            .push_bind(id)
            .push(")");
    }
    qb.push(" ORDER BY r.wa_timestamp DESC, r.id DESC LIMIT ");
    qb.push_bind(limit + 1);

    let rows = qb.build().fetch_all(pool.get_ref()).await?;
    let has_more = rows.len() as i64 > limit;
    let data: Vec<MessageView> = rows.iter().take(limit as usize).map(row_to_view).collect();
    let next_cursor = if has_more {
        data.last()
            .map(|m| format!("{}:{}", m.wa_timestamp.timestamp_millis(), m.id))
    } else {
        None
    };

    Ok(HttpResponse::Ok().json(MessagePage { data, next_cursor }))
}

pub async fn get(pool: web::Data<PgPool>, path: web::Path<i64>) -> AppResult<HttpResponse> {
    let id = path.into_inner();
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(SELECT);
    qb.push(" WHERE r.id = ").push_bind(id);
    let row = qb
        .build()
        .fetch_optional(pool.get_ref())
        .await?
        .ok_or_else(|| AppError::NotFound(format!("message {id}")))?;
    Ok(HttpResponse::Ok().json(row_to_view(&row)))
}
