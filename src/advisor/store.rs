//! Persistence for the advisor schema (migration 20261024210000_advisor).

use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use super::actions::Planned;
use super::llm::Message;

/// The transcript replayed per turn is capped: older messages beyond this are
/// still stored and shown, but the model sees the most recent ones. Cut only at
/// a user message, so a tool result is never separated from its call.
const REPLAY_LIMIT: i64 = 60;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ConversationRow {
    pub id: Uuid,
    pub title: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

pub async fn create_conversation(pool: &PgPool, user_id: i64, title: &str) -> sqlx::Result<Uuid> {
    let id = Uuid::new_v4();
    let title: String = title.chars().take(80).collect();
    sqlx::query("INSERT INTO advisor.conversations (id, user_id, title) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(user_id)
        .bind(title)
        .execute(pool)
        .await?;
    Ok(id)
}

/// A conversation is its owner's: another admin gets a 404, not someone
/// else's transcript.
pub async fn owns(pool: &PgPool, id: Uuid, user_id: i64) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM advisor.conversations
                         WHERE id = $1 AND user_id = $2 AND deleted_at IS NULL)",
    )
    .bind(id)
    .bind(user_id)
    .fetch_one(pool)
    .await
}

pub async fn list_conversations(pool: &PgPool, user_id: i64) -> sqlx::Result<Vec<ConversationRow>> {
    sqlx::query_as(
        "SELECT id, title, created_at, updated_at FROM advisor.conversations
          WHERE user_id = $1 AND deleted_at IS NULL
          ORDER BY updated_at DESC LIMIT 100",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

pub async fn delete_conversation(pool: &PgPool, id: Uuid, user_id: i64) -> sqlx::Result<bool> {
    let r = sqlx::query(
        "UPDATE advisor.conversations SET deleted_at = now()
          WHERE id = $1 AND user_id = $2 AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

pub async fn append(pool: &PgPool, conversation_id: Uuid, messages: &[Message]) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    for m in messages {
        sqlx::query("INSERT INTO advisor.messages (conversation_id, message) VALUES ($1, $2)")
            .bind(conversation_id)
            .bind(serde_json::to_value(m).unwrap_or(Value::Null))
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("UPDATE advisor.conversations SET updated_at = now() WHERE id = $1")
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

/// Every stored message, oldest first.
pub async fn transcript(pool: &PgPool, conversation_id: Uuid) -> sqlx::Result<Vec<Message>> {
    let rows: Vec<(Value,)> = sqlx::query_as(
        "SELECT message FROM advisor.messages WHERE conversation_id = $1 ORDER BY id",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(v,)| serde_json::from_value(v).ok())
        .collect())
}

/// The tail the model replays: at most REPLAY_LIMIT messages, starting at a
/// user message.
pub fn replay_window(all: Vec<Message>) -> Vec<Message> {
    let limit = REPLAY_LIMIT as usize;
    if all.len() <= limit {
        return all;
    }
    let mut start = all.len() - limit;
    while start < all.len() && !matches!(all[start], Message::User(_)) {
        start += 1;
    }
    all[start..].to_vec()
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct SuggestionRow {
    pub id: Uuid,
    pub conversation_id: Option<Uuid>,
    pub kind: String,
    pub title: String,
    pub rationale: String,
    pub before: Value,
    pub after: Value,
    pub target: String,
    pub method: String,
    pub path: String,
    pub body: Value,
    pub status: String,
    pub proposed_for_user_id: i64,
    pub decided_by_user_id: Option<i64>,
    pub decided_at: Option<chrono::DateTime<chrono::Utc>>,
    pub applied_at: Option<chrono::DateTime<chrono::Utc>>,
    pub result_status: Option<i32>,
    pub result: Option<Value>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub struct NewSuggestion<'a> {
    pub conversation_id: Option<Uuid>,
    pub user_id: i64,
    pub title: &'a str,
    pub rationale: &'a str,
    pub before: Value,
    pub after: Value,
    pub planned: &'a Planned,
}

pub async fn insert_suggestion(pool: &PgPool, s: NewSuggestion<'_>) -> sqlx::Result<Uuid> {
    let id = Uuid::new_v4();
    let target = match s.planned.target {
        super::upstream::Target::Falcon => "falcon",
        super::upstream::Target::Maint => "maint",
    };
    sqlx::query(
        "INSERT INTO advisor.suggestions
           (id, conversation_id, kind, title, rationale, before, after,
            target, method, path, body, proposed_for_user_id)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
    )
    .bind(id)
    .bind(s.conversation_id)
    .bind(s.planned.kind)
    .bind(s.title)
    .bind(s.rationale)
    .bind(s.before)
    .bind(s.after)
    .bind(target)
    .bind(s.planned.method)
    .bind(&s.planned.path)
    .bind(&s.planned.body)
    .bind(s.user_id)
    .execute(pool)
    .await?;
    Ok(id)
}

pub async fn suggestions(
    pool: &PgPool,
    status: Option<&str>,
    conversation_id: Option<Uuid>,
) -> sqlx::Result<Vec<SuggestionRow>> {
    sqlx::query_as(
        "SELECT * FROM advisor.suggestions
          WHERE ($1::text IS NULL OR status = $1)
            AND ($2::uuid IS NULL OR conversation_id = $2)
          ORDER BY created_at DESC LIMIT 200",
    )
    .bind(status)
    .bind(conversation_id)
    .fetch_all(pool)
    .await
}

pub async fn suggestion(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<SuggestionRow>> {
    sqlx::query_as("SELECT * FROM advisor.suggestions WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Move a pending suggestion to `applying` (approve) or `rejected`. The
/// status guard makes a double-click or two admins racing apply it once.
pub async fn decide(
    pool: &PgPool,
    id: Uuid,
    user_id: i64,
    to: &str,
) -> sqlx::Result<Option<SuggestionRow>> {
    sqlx::query_as(
        "UPDATE advisor.suggestions
            SET status = $3, decided_by_user_id = $2, decided_at = now()
          WHERE id = $1 AND status = 'pending'
         RETURNING *",
    )
    .bind(id)
    .bind(user_id)
    .bind(to)
    .fetch_optional(pool)
    .await
}

pub async fn record_result(
    pool: &PgPool,
    id: Uuid,
    ok: bool,
    status: Option<u16>,
    result: &Value,
) -> sqlx::Result<SuggestionRow> {
    sqlx::query_as(
        "UPDATE advisor.suggestions
            SET status = CASE WHEN $2 THEN 'applied' ELSE 'failed' END,
                applied_at = CASE WHEN $2 THEN now() END,
                result_status = $3, result = $4
          WHERE id = $1
         RETURNING *",
    )
    .bind(id)
    .bind(ok)
    .bind(status.map(i32::from))
    .bind(result)
    .fetch_one(pool)
    .await
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct DigestRow {
    pub id: Uuid,
    pub period_start: chrono::NaiveDate,
    pub period_end: chrono::NaiveDate,
    pub body: String,
    pub model: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub async fn save_digest(
    pool: &PgPool,
    start: chrono::NaiveDate,
    end: chrono::NaiveDate,
    body: &str,
    model: &str,
) -> sqlx::Result<Uuid> {
    sqlx::query_scalar(
        "INSERT INTO advisor.digests (id, period_start, period_end, body, model)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (period_start, period_end)
         DO UPDATE SET body = EXCLUDED.body, model = EXCLUDED.model, created_at = now()
         RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(start)
    .bind(end)
    .bind(body)
    .bind(model)
    .fetch_one(pool)
    .await
}

pub async fn digests(pool: &PgPool) -> sqlx::Result<Vec<DigestRow>> {
    sqlx::query_as("SELECT * FROM advisor.digests ORDER BY period_end DESC, created_at DESC LIMIT 60")
        .fetch_all(pool)
        .await
}

pub async fn digest_exists(pool: &PgPool, start: chrono::NaiveDate, end: chrono::NaiveDate) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM advisor.digests WHERE period_start = $1 AND period_end = $2)",
    )
    .bind(start)
    .bind(end)
    .fetch_one(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_replay_window_starts_at_a_user_message() {
        let mut all = vec![Message::User("q0".into())];
        for i in 0..70 {
            all.push(Message::Assistant { text: Some(format!("a{i}")), calls: vec![] });
            all.push(Message::User(format!("q{}", i + 1)));
        }
        let w = replay_window(all);
        assert!(w.len() <= REPLAY_LIMIT as usize);
        assert!(matches!(w[0], Message::User(_)));
    }

    #[test]
    fn messages_round_trip_through_json() {
        let m = Message::ToolResult {
            id: "t1".into(),
            name: "run_sql".into(),
            content: serde_json::json!({ "row_count": 1 }),
        };
        let back: Message = serde_json::from_value(serde_json::to_value(&m).unwrap()).unwrap();
        assert!(matches!(back, Message::ToolResult { ref name, .. } if name == "run_sql"));
    }
}
