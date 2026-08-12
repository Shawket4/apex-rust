//! The poller: the ONE ingest path.
//!
//! Every message enters through `process_batch` — the poller, the backfill CLI
//! and the reparse sweep all share it. Store raw first (verbatim), verdict from
//! the parser, transaction inserted in the same per-page transaction. The
//! cursor advances once per cycle, after all pages commit (see `cursor.rs` for
//! why that ordering is the whole silent-skip fix).
//!
//! There is no webhook. At 3-8 bank SMS per day, a 60-second poll is the
//! latency floor anyone can perceive, and the poller is the delivery
//! *guarantee*; the old webhook was 216 lines buying less than a minute.

use chrono::{DateTime, Utc};
use log::{error, info, warn};
use sqlx::{PgPool, Row};
use std::time::Duration;

use super::cursor;
use super::whatsapp_client::{WaMessage, WhatsAppClient, MAX_PAGE_LIMIT};
use crate::config::CONFIG;
use crate::errors::AppResult;
use crate::ops::notify::{self, NewTransaction};
use crate::parser::{self, templates::CompiledTemplate, Verdict};

/// Hard ceiling on pages per cycle; a runaway loop stops here and the next
/// cycle continues from wherever the cursor still points.
const MAX_PAGES_PER_CYCLE: u32 = 200;

/// One new transaction created by an ingest pass, for notifications.
pub struct Created {
    pub txn: NewTransaction,
}

/// Insert a batch of messages and their verdicts, one DB transaction.
///
/// Returns the transactions actually created (only for raws that were NEW —
/// a message seen before is never re-parsed here, so human edits and
/// promotions are never disturbed).
pub async fn process_batch(
    pool: &PgPool,
    templates: &[CompiledTemplate],
    messages: &[WaMessage],
) -> AppResult<Vec<Created>> {
    if messages.is_empty() {
        return Ok(Vec::new());
    }

    // Verdict per message, computed up front (pure CPU, no I/O).
    let verdicts: Vec<Verdict> = messages
        .iter()
        .map(|m| parser::parse(&m.content, templates, Some(m.timestamp)))
        .collect();

    let mut tx = pool.begin().await?;

    // Batch-insert raws via UNNEST; RETURNING tells us which were genuinely new.
    let ids: Vec<&str> = messages.iter().map(|m| m.id.as_str()).collect();
    let chat_jids: Vec<&str> = messages.iter().map(|m| m.chat_jid.as_str()).collect();
    let senders: Vec<Option<&str>> = messages.iter().map(|m| m.sender_jid.as_deref()).collect();
    let from_me: Vec<bool> = messages.iter().map(|m| m.is_from_me).collect();
    let timestamps: Vec<DateTime<Utc>> = messages.iter().map(|m| m.timestamp).collect();
    let bodies: Vec<&str> = messages.iter().map(|m| m.content.as_str()).collect();
    let media: Vec<Option<&str>> = messages.iter().map(|m| m.media_type.as_deref()).collect();
    let statuses: Vec<&str> = verdicts.iter().map(|v| v.status()).collect();
    let templates_used: Vec<Option<&str>> = verdicts.iter().map(|v| v.template()).collect();

    let inserted = sqlx::query(
        r#"
        INSERT INTO banksms.raw_messages
            (wa_message_id, chat_jid, sender, is_from_me, wa_timestamp, body,
             media_type, status, template)
        SELECT * FROM UNNEST(
            $1::text[], $2::text[], $3::text[], $4::boolean[], $5::timestamptz[],
            $6::text[], $7::text[], $8::text[], $9::text[])
        ON CONFLICT (wa_message_id) DO NOTHING
        RETURNING id, wa_message_id
        "#,
    )
    .bind(&ids)
    .bind(&chat_jids)
    .bind(&senders)
    .bind(&from_me)
    .bind(&timestamps)
    .bind(&bodies)
    .bind(&media)
    .bind(&statuses)
    .bind(&templates_used)
    .fetch_all(&mut *tx)
    .await?;

    let mut created = Vec::new();
    for row in &inserted {
        let raw_id: i64 = row.get("id");
        let wa_id: &str = row.get("wa_message_id");
        let idx = messages.iter().position(|m| m.id == wa_id).unwrap();
        if let Verdict::Matched { template, fields } = &verdicts[idx] {
            let txn = sqlx::query(
                r#"
                INSERT INTO banksms.transactions
                    (source, raw_message_id, direction, amount, currency, occurred_at,
                     account, counterparty, reference, created_by)
                VALUES ('whatsapp', $1, $2, $3, $4, $5, $6, $7, $8, 'parser')
                ON CONFLICT (raw_message_id) DO NOTHING
                RETURNING id
                "#,
            )
            .bind(raw_id)
            .bind(&fields.direction)
            .bind(fields.amount)
            .bind(&fields.currency)
            .bind(fields.occurred_at)
            .bind(&fields.account)
            .bind(&fields.counterparty)
            .bind(&fields.reference)
            .fetch_optional(&mut *tx)
            .await?;

            if let Some(t) = txn {
                created.push(Created {
                    txn: NewTransaction {
                        id: t.get("id"),
                        amount: Some(fields.amount),
                        currency: Some(fields.currency.clone()),
                        direction: Some(fields.direction.clone()),
                        counterparty: fields.counterparty.clone(),
                        account: fields.account.clone(),
                        reference: fields.reference.clone(),
                        template: Some(template.clone()),
                    },
                });
            }
        }
    }

    tx.commit().await?;
    Ok(created)
}

/// One poll cycle: page from `cursor − overlap` until a short page, insert
/// every page, then advance the cursor once.
pub async fn poll_once(pool: &PgPool, client: &WhatsAppClient) -> AppResult<Vec<Created>> {
    let chat_jid = &CONFIG.target_chat_jid;
    let templates = parser::templates::load(pool).await?;

    let cur = cursor::load(pool).await?;
    let start_time = cur.poll_from();

    let mut newest: Option<(DateTime<Utc>, String)> = None;
    let mut all_created = Vec::new();
    let mut offset = 0u32;
    let mut pages = 0u32;

    loop {
        let page = client
            .list_messages(chat_jid, start_time, MAX_PAGE_LIMIT, offset)
            .await?;
        let n = page.len() as u32;
        if n == 0 {
            break;
        }

        // Defensive: the API is filtered by path, but a message from another
        // chat must never enter this table.
        let page: Vec<WaMessage> = page
            .into_iter()
            .filter(|m| &m.chat_jid == chat_jid)
            .collect();

        for m in &page {
            match &newest {
                Some((ts, _)) if *ts >= m.timestamp => {}
                _ => newest = Some((m.timestamp, m.id.clone())),
            }
        }

        all_created.extend(process_batch(pool, &templates, &page).await?);

        pages += 1;
        if n < MAX_PAGE_LIMIT || pages >= MAX_PAGES_PER_CYCLE {
            if pages >= MAX_PAGES_PER_CYCLE {
                warn!(
                    "poll cycle hit the {MAX_PAGES_PER_CYCLE}-page ceiling; continuing next cycle"
                );
            }
            break;
        }
        offset += MAX_PAGE_LIMIT;
    }

    // The cursor advances only now, after every page above has committed.
    match &newest {
        Some((ts, id)) => cursor::advance(pool, chat_jid, *ts, id).await?,
        None => cursor::touch_poll(pool, chat_jid).await?,
    }

    Ok(all_created)
}

/// The forever loop. Exponential backoff with full jitter on errors.
pub async fn run(pool: PgPool, client: WhatsAppClient) {
    if CONFIG.target_chat_jid.is_empty() {
        warn!("TARGET_CHAT_JID is empty — poller not started");
        return;
    }
    info!(
        "poller started: chat={} every {}s, overlap {}s",
        CONFIG.target_chat_jid, CONFIG.poll_interval_secs, CONFIG.overlap_window_secs
    );

    let mut consecutive_failures = 0u32;
    loop {
        match poll_once(&pool, &client).await {
            Ok(created) => {
                consecutive_failures = 0;
                if !created.is_empty() {
                    info!("poll cycle created {} transaction(s)", created.len());
                    let txns: Vec<NewTransaction> = created.into_iter().map(|c| c.txn).collect();
                    notify::notify_new_transactions(txns);
                }
                tokio::time::sleep(Duration::from_secs(CONFIG.poll_interval_secs)).await;
            }
            Err(e) => {
                consecutive_failures += 1;
                error!("poll cycle failed ({consecutive_failures} in a row): {e}");
                let _ = cursor::record_error(&pool, &CONFIG.target_chat_jid, &e.to_string()).await;
                tokio::time::sleep(backoff_delay(consecutive_failures)).await;
            }
        }
    }
}

/// Exponential backoff with full jitter, capped at POLL_BACKOFF_MAX_SECS.
/// Jitter is derived from the clock's nanoseconds — good enough to de-align
/// retries without pulling in a randomness crate.
fn backoff_delay(failures: u32) -> Duration {
    let base = CONFIG.poll_interval_secs.max(1);
    let exp = base.saturating_mul(2u64.saturating_pow(failures.min(6)));
    let capped = exp.min(CONFIG.poll_backoff_max_secs.max(base));
    let span = capped.saturating_sub(base).max(1);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    Duration::from_secs(base + nanos % span)
}

/// One-shot full-history backfill (`apex-rust backfill`). Same insert path,
/// never touches the cursor — the cursor tracks the live tail.
pub async fn backfill(pool: &PgPool, client: &WhatsAppClient) -> AppResult<usize> {
    let chat_jid = &CONFIG.target_chat_jid;
    let templates = parser::templates::load(pool).await?;
    let mut total = 0usize;
    let mut offset = 0u32;

    loop {
        let page = client
            .list_messages(chat_jid, None, MAX_PAGE_LIMIT, offset)
            .await?;
        let n = page.len() as u32;
        if n == 0 {
            break;
        }
        let page: Vec<WaMessage> = page
            .into_iter()
            .filter(|m| &m.chat_jid == chat_jid)
            .collect();
        total += process_batch(pool, &templates, &page).await?.len();
        if n < MAX_PAGE_LIMIT {
            break;
        }
        offset += MAX_PAGE_LIMIT;
    }
    Ok(total)
}

/// Re-run the parser over stored raws that never produced a transaction
/// (`apex-rust reparse`, and after template writes). This is what makes
/// "status is recomputable" true in practice: seed a new template row, run
/// this, and the backlog it covers becomes transactions. Idempotent — a raw
/// with a transaction (parsed or human-promoted) is never touched.
pub async fn reparse_sweep(pool: &PgPool) -> AppResult<(usize, usize)> {
    let templates = parser::templates::load(pool).await?;

    let rows = sqlx::query(
        r#"
        SELECT r.id, r.body, r.wa_timestamp, r.status, r.template
        FROM banksms.raw_messages r
        LEFT JOIN banksms.transactions t ON t.raw_message_id = r.id
        WHERE t.id IS NULL
        ORDER BY r.id
        "#,
    )
    .fetch_all(pool)
    .await?;

    let mut status_changes = 0usize;
    let mut created = 0usize;

    for r in rows {
        let raw_id: i64 = r.get("id");
        let body: String = r.get("body");
        let wa_ts: DateTime<Utc> = r.get("wa_timestamp");
        let old_status: String = r.get("status");
        let old_template: Option<String> = r.get("template");

        let verdict = parser::parse(&body, &templates, Some(wa_ts));

        let mut tx = pool.begin().await?;
        if verdict.status() != old_status || verdict.template() != old_template.as_deref() {
            sqlx::query("UPDATE banksms.raw_messages SET status = $1, template = $2 WHERE id = $3")
                .bind(verdict.status())
                .bind(verdict.template())
                .bind(raw_id)
                .execute(&mut *tx)
                .await?;
            status_changes += 1;
        }
        if let Verdict::Matched { fields, .. } = &verdict {
            let done = sqlx::query(
                r#"
                INSERT INTO banksms.transactions
                    (source, raw_message_id, direction, amount, currency, occurred_at,
                     account, counterparty, reference, created_by)
                VALUES ('whatsapp', $1, $2, $3, $4, $5, $6, $7, $8, 'parser')
                ON CONFLICT (raw_message_id) DO NOTHING
                RETURNING id
                "#,
            )
            .bind(raw_id)
            .bind(&fields.direction)
            .bind(fields.amount)
            .bind(&fields.currency)
            .bind(fields.occurred_at)
            .bind(&fields.account)
            .bind(&fields.counterparty)
            .bind(&fields.reference)
            .fetch_optional(&mut *tx)
            .await?;
            if done.is_some() {
                created += 1;
            }
        }
        tx.commit().await?;
    }

    Ok((status_changes, created))
}
