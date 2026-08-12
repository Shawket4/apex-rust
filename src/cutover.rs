//! `apex-rust cutover` — the one-shot production migration from the legacy
//! banksms schema to v2. Run while the HTTP server is stopped.
//!
//! Everything happens in ONE database transaction:
//!
//!   1. preflight (legacy present, not already cut over)
//!   2. ALTER SCHEMA banksms RENAME TO banksms_legacy
//!   3. execute the v2 baseline DDL + seeds, stamp banksms._sqlx_migrations
//!      so sqlx::migrate! sees the baseline as applied
//!   4. copy raw messages, ids preserved, status recomputed by the new parser
//!   5. copy transactions, ids/versions/timestamps preserved, ALL active
//!      overrides folded generically, the two direction-less partials
//!      re-parsed to completion by the new templates
//!   6. restart both identity sequences past max(id)
//!   7. verify — parity with the legacy schema plus immutable spot checks —
//!      and ABORT (full rollback, legacy schema untouched) on any mismatch
//!
//! Verification is parity-based, not a hardcoded snapshot: production keeps
//! ingesting between the day this was written and the day it runs. The spot
//! checks cover history that cannot change (the three override-corrected
//! rows, the two stuck partials, the migration-baseline totals as floors).

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sha2::{Digest, Sha384};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::collections::HashMap;
use std::str::FromStr;

use crate::errors::{AppError, AppResult};
use crate::parser::{self, templates::CompiledTemplate, Verdict};

const BASELINE_VERSION: i64 = 20260813000000;
const BASELINE_DESCRIPTION: &str = "banksms v2";
const BASELINE_SQL: &str = include_str!("../migrations/20260813000000_banksms_v2.up.sql");

/// Immutable floors from the 2026-08-13 baseline dump. Live counts can only
/// have grown since; falling below any of these means data was lost.
const FLOOR_LIVE_TXNS: i64 = 315;
const FLOOR_LIVE_SUM: &str = "2921440.35";
const FLOOR_IMPORT_TXNS: i64 = 223;
const IMPORT_SUM: &str = "2488598.00"; // imports stopped in Feb 2026 — exact, not a floor
const FLOOR_RAWS: i64 = 815;

macro_rules! ensure {
    ($cond:expr, $($msg:tt)*) => {
        if !($cond) {
            return Err(AppError::Internal(format!("CUTOVER VERIFY FAILED: {}", format!($($msg)*))));
        }
    };
}

/// The baseline floors assume production data. Rehearsals against synthetic
/// fixtures set CUTOVER_SKIP_FLOORS=1 (loudly); the production runbook never
/// does.
fn skip_floors() -> bool {
    let skip = std::env::var("CUTOVER_SKIP_FLOORS")
        .map(|v| v == "1")
        .unwrap_or(false);
    if skip {
        log::warn!("CUTOVER_SKIP_FLOORS=1 — baseline floor checks are OFF (rehearsal mode)");
    }
    skip
}

pub async fn run(pool: &PgPool) -> AppResult<()> {
    let mut tx = pool.begin().await?;

    preflight(&mut tx).await?;
    log::info!("cutover: renaming banksms -> banksms_legacy");
    sqlx::query("ALTER SCHEMA banksms RENAME TO banksms_legacy")
        .execute(&mut *tx)
        .await?;

    log::info!("cutover: creating v2 schema + seeds");
    sqlx::raw_sql(BASELINE_SQL).execute(&mut *tx).await?;
    stamp_migrations(&mut tx).await?;

    let templates = load_templates_in_tx(&mut tx).await?;
    ensure!(
        templates.len() == 8,
        "expected 8 seeded templates, got {}",
        templates.len()
    );

    log::info!("cutover: copying raw messages");
    let raw_stats = copy_raws(&mut tx, &templates).await?;
    log::info!(
        "cutover: raws copied — {} total: {} matched / {} suppressed / {} ignored",
        raw_stats.total,
        raw_stats.matched,
        raw_stats.suppressed,
        raw_stats.ignored
    );

    log::info!("cutover: copying transactions with override fold");
    let txn_stats = copy_transactions(&mut tx, &templates).await?;
    log::info!(
        "cutover: transactions copied — {} total ({} reparsed partials, {} with edits folded)",
        txn_stats.total,
        txn_stats.reparsed,
        txn_stats.edited
    );

    log::info!("cutover: restarting identity sequences");
    for table in ["raw_messages", "transactions"] {
        sqlx::query(&format!(
            "SELECT setval(pg_get_serial_sequence('banksms.{table}', 'id'),
                    (SELECT COALESCE(MAX(id), 0) + 1 FROM banksms.{table}), false)"
        ))
        .execute(&mut *tx)
        .await?;
    }

    log::info!("cutover: verifying");
    verify(&mut tx, &raw_stats, &txn_stats).await?;

    tx.commit().await?;
    log::info!(
        "cutover: COMMITTED. banksms_legacy retained for the soak; drop it after two clean weeks."
    );
    Ok(())
}

async fn preflight(tx: &mut Transaction<'_, Postgres>) -> AppResult<()> {
    let legacy_marker: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('banksms.transaction_overrides')::text")
            .fetch_one(&mut **tx)
            .await?;
    if legacy_marker.is_none() {
        return Err(AppError::Internal(
            "preflight: banksms.transaction_overrides not found — either the cutover \
             already ran or this database never had the legacy schema"
                .into(),
        ));
    }
    let already: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('banksms_legacy.raw_messages')::text")
            .fetch_one(&mut **tx)
            .await?;
    if already.is_some() {
        return Err(AppError::Internal(
            "preflight: banksms_legacy already exists — refusing to run twice".into(),
        ));
    }
    Ok(())
}

/// Create the bookkeeping table exactly as sqlx 0.8 would, and stamp the
/// baseline as applied with the checksum sqlx will verify at every boot.
/// The integration suite proves migrate! treats this as a no-op afterwards.
async fn stamp_migrations(tx: &mut Transaction<'_, Postgres>) -> AppResult<()> {
    sqlx::query(
        r#"
        CREATE TABLE banksms._sqlx_migrations (
            version        BIGINT PRIMARY KEY,
            description    TEXT NOT NULL,
            installed_on   TIMESTAMPTZ NOT NULL DEFAULT now(),
            success        BOOLEAN NOT NULL,
            checksum       BYTEA NOT NULL,
            execution_time BIGINT NOT NULL
        )
        "#,
    )
    .execute(&mut **tx)
    .await?;

    let checksum: Vec<u8> = Sha384::digest(BASELINE_SQL.as_bytes()).to_vec();
    sqlx::query(
        "INSERT INTO banksms._sqlx_migrations
             (version, description, success, checksum, execution_time)
         VALUES ($1, $2, TRUE, $3, 0)",
    )
    .bind(BASELINE_VERSION)
    .bind(BASELINE_DESCRIPTION)
    .bind(&checksum)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn load_templates_in_tx(
    tx: &mut Transaction<'_, Postgres>,
) -> AppResult<Vec<CompiledTemplate>> {
    let rows = sqlx::query(
        "SELECT id, name, pattern, date_formats, direction_map, sample, priority
         FROM banksms.parse_templates WHERE enabled ORDER BY priority, id",
    )
    .fetch_all(&mut **tx)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let pattern: String = r.get("pattern");
        out.push(CompiledTemplate {
            id: r.get("id"),
            name: r.get("name"),
            regex: regex::Regex::new(&pattern)
                .map_err(|e| AppError::Internal(format!("seed template does not compile: {e}")))?,
            date_formats: serde_json::from_value(r.get("date_formats")).unwrap_or_default(),
            direction_map: serde_json::from_value(r.get("direction_map")).unwrap_or_default(),
            sample: r.get("sample"),
            priority: r.get("priority"),
        });
    }
    Ok(out)
}

pub struct RawStats {
    pub total: i64,
    pub matched: i64,
    pub suppressed: i64,
    pub ignored: i64,
    /// legacy parse_status per raw id, for the parity checks
    pub legacy_status: HashMap<i64, String>,
}

async fn copy_raws(
    tx: &mut Transaction<'_, Postgres>,
    templates: &[CompiledTemplate],
) -> AppResult<RawStats> {
    let rows = sqlx::query(
        "SELECT id, wa_message_id, chat_jid, sender, is_from_me, wa_timestamp, body,
                parse_status::text AS parse_status, ingested_at
         FROM banksms_legacy.raw_messages ORDER BY id",
    )
    .fetch_all(&mut **tx)
    .await?;

    let mut stats = RawStats {
        total: 0,
        matched: 0,
        suppressed: 0,
        ignored: 0,
        legacy_status: HashMap::new(),
    };

    for r in &rows {
        let id: i64 = r.get("id");
        let body: String = r.get("body");
        let wa_ts: DateTime<Utc> = r.get("wa_timestamp");
        let verdict = parser::parse(&body, templates, Some(wa_ts));
        match &verdict {
            Verdict::Matched { .. } => stats.matched += 1,
            Verdict::Suppressed { .. } => stats.suppressed += 1,
            Verdict::Ignored => stats.ignored += 1,
        }
        stats.legacy_status.insert(id, r.get("parse_status"));

        sqlx::query(
            "INSERT INTO banksms.raw_messages
                 (id, wa_message_id, chat_jid, sender, is_from_me, wa_timestamp,
                  body, media_type, status, template, ingested_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, NULL, $8, $9, $10)",
        )
        .bind(id)
        .bind(r.get::<String, _>("wa_message_id"))
        .bind(r.get::<String, _>("chat_jid"))
        .bind(r.get::<Option<String>, _>("sender"))
        .bind(r.get::<bool, _>("is_from_me"))
        .bind(wa_ts)
        .bind(&body)
        .bind(verdict.status())
        .bind(verdict.template())
        .bind(r.get::<DateTime<Utc>, _>("ingested_at"))
        .execute(&mut **tx)
        .await?;
        stats.total += 1;
    }
    Ok(stats)
}

pub struct TxnStats {
    pub total: i64,
    pub reparsed: i64,
    pub edited: i64,
    pub legacy_live_count: i64,
    pub legacy_live_sum: Decimal,
    pub legacy_postings: i64,
}

async fn copy_transactions(
    tx: &mut Transaction<'_, Postgres>,
    templates: &[CompiledTemplate],
) -> AppResult<TxnStats> {
    // Effective values: EVERY overridable field folded generically from active
    // overrides, not just the ones believed to differ. edited_by/edited_at
    // come from the latest active override so edit provenance survives.
    let rows = sqlx::query(
        r#"
        SELECT t.id, t.source::text AS source, t.raw_message_id, t.import_source_id,
               COALESCE(o.direction, t.parsed_direction::text)      AS direction,
               COALESCE(o.amount::numeric, t.parsed_amount)         AS amount,
               COALESCE(o.currency, t.parsed_currency)              AS currency,
               COALESCE(o.account, t.parsed_account)                AS account,
               COALESCE(o.counterparty, t.parsed_counterparty)      AS counterparty,
               COALESCE(o.reference, t.parsed_reference)            AS reference,
               COALESCE(o.occurred_at::timestamptz, t.parsed_occurred_at, t.created_at)
                                                                    AS occurred_at,
               NULLIF(t.category, '')                               AS category,
               t.description, t.payment_method, t.company, t.car_id, t.car_no_plate,
               t.driver_id, t.employee_id, t.paid_by, t.version, t.created_by,
               t.created_at, t.updated_at, t.deleted_at,
               o.actor AS edit_actor, o.set_at AS edit_at,
               r.body AS raw_body, r.wa_timestamp AS raw_ts
        FROM banksms_legacy.transactions t
        LEFT JOIN banksms_legacy.raw_messages r ON r.id = t.raw_message_id
        LEFT JOIN LATERAL (
            SELECT MAX(CASE WHEN ov.field = 'direction'    THEN ov.value END) AS direction,
                   MAX(CASE WHEN ov.field = 'amount'       THEN ov.value END) AS amount,
                   MAX(CASE WHEN ov.field = 'currency'     THEN ov.value END) AS currency,
                   MAX(CASE WHEN ov.field = 'account'      THEN ov.value END) AS account,
                   MAX(CASE WHEN ov.field = 'counterparty' THEN ov.value END) AS counterparty,
                   MAX(CASE WHEN ov.field = 'reference'    THEN ov.value END) AS reference,
                   MAX(CASE WHEN ov.field = 'occurred_at'  THEN ov.value END) AS occurred_at,
                   MAX(ov.actor)  AS actor,
                   MAX(ov.set_at) AS set_at
            FROM banksms_legacy.transaction_overrides ov
            WHERE ov.transaction_id = t.id
              AND ov.superseded_at IS NULL AND NOT ov.is_cleared
        ) o ON TRUE
        ORDER BY t.id
        "#,
    )
    .fetch_all(&mut **tx)
    .await?;

    let mut stats = TxnStats {
        total: 0,
        reparsed: 0,
        edited: 0,
        legacy_live_count: 0,
        legacy_live_sum: Decimal::ZERO,
        legacy_postings: 0,
    };

    for r in &rows {
        let id: i64 = r.get("id");
        let deleted_at: Option<DateTime<Utc>> = r.get("deleted_at");
        let mut direction: Option<String> = r.get("direction");
        let mut amount: Option<Decimal> = r.get("amount");
        let mut currency: Option<String> = r.get("currency");
        let mut account: Option<String> = r.get("account");
        let mut counterparty: Option<String> = r.get("counterparty");
        let mut reference: Option<String> = r.get("reference");
        let mut occurred_at: DateTime<Utc> = r.get("occurred_at");

        // The direction-less partials: re-parse the raw body with the new
        // template set instead of guessing. If the new templates cannot
        // complete them, the whole cutover aborts — by design.
        if direction.is_none() {
            let raw_body: Option<String> = r.get("raw_body");
            let raw_ts: Option<DateTime<Utc>> = r.get("raw_ts");
            let body = raw_body.ok_or_else(|| {
                AppError::Internal(format!(
                    "CUTOVER: transaction {id} has no direction and no raw body to re-parse"
                ))
            })?;
            match parser::parse(&body, templates, raw_ts) {
                Verdict::Matched { fields, .. } => {
                    let legacy_amount = amount;
                    ensure!(
                        legacy_amount.is_none() || legacy_amount == Some(fields.amount),
                        "re-parse of txn {id} changed the amount: {:?} -> {}",
                        legacy_amount,
                        fields.amount
                    );
                    direction = Some(fields.direction);
                    amount = Some(fields.amount);
                    currency = Some(fields.currency);
                    account = fields.account.or(account);
                    counterparty = fields.counterparty.or(counterparty);
                    reference = fields.reference.or(reference);
                    occurred_at = fields.occurred_at;
                    stats.reparsed += 1;
                }
                other => {
                    return Err(AppError::Internal(format!(
                        "CUTOVER: transaction {id} is incomplete and its message re-parses \
                         as {:?} — a template is missing; aborting",
                        other.status()
                    )));
                }
            }
        }

        let direction = direction.ok_or_else(|| {
            AppError::Internal(format!("CUTOVER: transaction {id} still has no direction"))
        })?;
        let amount = amount.ok_or_else(|| {
            AppError::Internal(format!("CUTOVER: transaction {id} has no amount"))
        })?;
        let edit_actor: Option<String> = r.get("edit_actor");
        if edit_actor.is_some() {
            stats.edited += 1;
        }

        if deleted_at.is_none() {
            stats.legacy_live_count += 1;
            stats.legacy_live_sum += amount;
        }

        sqlx::query(
            r#"
            INSERT INTO banksms.transactions
                (id, source, raw_message_id, import_source_id, direction, amount,
                 currency, occurred_at, account, counterparty, reference, category,
                 description, payment_method, company, car_id, car_no_plate,
                 driver_id, employee_id, paid_by, version, created_by,
                 edited_by, edited_at, deleted_at, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                    $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27)
            "#,
        )
        .bind(id)
        .bind(r.get::<String, _>("source"))
        .bind(r.get::<Option<i64>, _>("raw_message_id"))
        .bind(r.get::<Option<i64>, _>("import_source_id"))
        .bind(&direction)
        .bind(amount)
        .bind(currency.as_deref().unwrap_or("EGP"))
        .bind(occurred_at)
        .bind(&account)
        .bind(&counterparty)
        .bind(&reference)
        .bind(r.get::<Option<String>, _>("category"))
        .bind(r.get::<Option<String>, _>("description"))
        .bind(r.get::<Option<String>, _>("payment_method"))
        .bind(r.get::<Option<String>, _>("company"))
        .bind(r.get::<Option<i64>, _>("car_id"))
        .bind(r.get::<Option<String>, _>("car_no_plate"))
        .bind(r.get::<Option<i64>, _>("driver_id"))
        .bind(r.get::<Option<i64>, _>("employee_id"))
        .bind(r.get::<Option<String>, _>("paid_by"))
        .bind(r.get::<i32, _>("version"))
        .bind(r.get::<Option<String>, _>("created_by"))
        .bind(&edit_actor)
        .bind(r.get::<Option<DateTime<Utc>>, _>("edit_at"))
        .bind(deleted_at)
        .bind(r.get::<DateTime<Utc>, _>("created_at"))
        .bind(r.get::<DateTime<Utc>, _>("updated_at"))
        .execute(&mut **tx)
        .await?;
        stats.total += 1;
    }

    // Any postings that appeared since the baseline dump become loan links.
    let postings = sqlx::query(
        "SELECT transaction_id, target_id FROM banksms_legacy.transaction_postings
         WHERE target_table = 'loans'",
    )
    .fetch_all(&mut **tx)
    .await?;
    stats.legacy_postings = postings.len() as i64;
    for p in &postings {
        sqlx::query("UPDATE banksms.transactions SET loan_id = $1 WHERE id = $2")
            .bind(p.get::<i64, _>("target_id"))
            .bind(p.get::<i64, _>("transaction_id"))
            .execute(&mut **tx)
            .await?;
    }

    // Copy the ingest cursor so the first post-cutover poll resumes exactly
    // where the legacy poller stopped.
    sqlx::query(
        "INSERT INTO banksms.ingest_cursor
             (id, chat_jid, last_wa_timestamp, last_wa_message_id, last_poll_at,
              consecutive_errors, updated_at)
         SELECT 1, COALESCE(chat_jid, ''), last_wa_timestamp, last_wa_message_id,
                last_poll_at, 0, now()
         FROM banksms_legacy.ingest_cursor WHERE id = 1",
    )
    .execute(&mut **tx)
    .await?;

    Ok(stats)
}

async fn verify(
    tx: &mut Transaction<'_, Postgres>,
    raws: &RawStats,
    txns: &TxnStats,
) -> AppResult<()> {
    // --- transactions: parity with the legacy schema --------------------
    let legacy_total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM banksms_legacy.transactions")
        .fetch_one(&mut **tx)
        .await?;
    let new_total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM banksms.transactions")
        .fetch_one(&mut **tx)
        .await?;
    ensure!(
        new_total == legacy_total,
        "txn count {new_total} != legacy {legacy_total}"
    );

    let (new_live, new_sum): (i64, Decimal) = {
        let r = sqlx::query(
            "SELECT COUNT(*) AS c, COALESCE(SUM(amount), 0)::numeric AS s
             FROM banksms.transactions WHERE deleted_at IS NULL",
        )
        .fetch_one(&mut **tx)
        .await?;
        (r.get("c"), r.get("s"))
    };
    ensure!(
        new_live == txns.legacy_live_count,
        "live txn count {new_live} != legacy effective {legacy}",
        legacy = txns.legacy_live_count
    );
    ensure!(
        new_sum == txns.legacy_live_sum,
        "live sum {new_sum} != legacy effective {legacy}",
        legacy = txns.legacy_live_sum
    );

    // Immutable floors from the baseline dump (production only).
    if !skip_floors() {
        ensure!(
            new_live >= FLOOR_LIVE_TXNS,
            "live count {new_live} below baseline floor"
        );
        let floor_sum = Decimal::from_str(FLOOR_LIVE_SUM).unwrap();
        ensure!(
            new_sum >= floor_sum,
            "live sum {new_sum} below baseline floor {floor_sum}"
        );

        // Per-source parity; imports are frozen so their numbers are exact.
        let import: (i64, Decimal) = {
            let r = sqlx::query(
                "SELECT COUNT(*) AS c, COALESCE(SUM(amount), 0)::numeric AS s
                 FROM banksms.transactions WHERE source = 'import' AND deleted_at IS NULL",
            )
            .fetch_one(&mut **tx)
            .await?;
            (r.get("c"), r.get("s"))
        };
        ensure!(
            import.0 == FLOOR_IMPORT_TXNS,
            "import count {} != 223",
            import.0
        );
        ensure!(
            import.1 == Decimal::from_str(IMPORT_SUM).unwrap(),
            "import sum {} != {IMPORT_SUM}",
            import.1
        );
    }

    // Per-category parity with the legacy schema (NULL and '' folded).
    let mismatches: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) FROM (
            SELECT COALESCE(NULLIF(category, ''), '(none)') AS k, COUNT(*) AS n
            FROM banksms_legacy.transactions WHERE deleted_at IS NULL GROUP BY 1
        ) legacy
        FULL OUTER JOIN (
            SELECT COALESCE(category, '(none)') AS k, COUNT(*) AS n
            FROM banksms.transactions WHERE deleted_at IS NULL GROUP BY 1
        ) v2 USING (k)
        WHERE legacy.n IS DISTINCT FROM v2.n
        "#,
    )
    .fetch_one(&mut **tx)
    .await?;
    ensure!(
        mismatches == 0,
        "{mismatches} per-category count mismatch(es)"
    );

    // --- raw messages ----------------------------------------------------
    if !skip_floors() {
        ensure!(
            raws.total >= FLOOR_RAWS,
            "raw count {} below baseline floor",
            raws.total
        );
    }
    let new_raws: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM banksms.raw_messages")
        .fetch_one(&mut **tx)
        .await?;
    ensure!(
        new_raws == raws.total,
        "copied raw count {new_raws} != {}",
        raws.total
    );

    // Every legacy 'parsed' or 'partial' raw must be 'matched' now — the new
    // parser is not allowed to lose anything the old one handled.
    let regressed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM banksms_legacy.raw_messages l
         JOIN banksms.raw_messages n ON n.id = l.id
         WHERE l.parse_status::text IN ('parsed', 'partial') AND n.status <> 'matched'",
    )
    .fetch_one(&mut **tx)
    .await?;
    ensure!(
        regressed == 0,
        "{regressed} previously-parsed message(s) regressed"
    );

    // Every matched raw has exactly one transaction (unique index gives ≤1;
    // this proves ≥1, soft-deleted included).
    let unmatched: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM banksms.raw_messages r
         LEFT JOIN banksms.transactions t ON t.raw_message_id = r.id
         WHERE r.status = 'matched' AND t.id IS NULL",
    )
    .fetch_one(&mut **tx)
    .await?;
    ensure!(
        unmatched == 0,
        "{unmatched} matched raw(s) without a transaction"
    );

    // --- immutable spot checks (history that cannot have changed; skipped
    // in rehearsal mode where those ids don't exist) -----------------------
    let spot_checks: &[(i64, &str)] = if skip_floors() {
        &[]
    } else {
        &[(224_i64, "9000"), (276, "13500"), (284, "3100")]
    };
    for &(id, amount) in spot_checks {
        let r = sqlx::query(
            "SELECT amount::text AS amount, occurred_at, edited_by, version
             FROM banksms.transactions WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
        let r = r.ok_or_else(|| AppError::Internal(format!("VERIFY: txn {id} missing")))?;
        let got: String = r.get("amount");
        ensure!(
            Decimal::from_str(&got).unwrap() == Decimal::from_str(amount).unwrap(),
            "txn {id} amount {got} != {amount}"
        );
        let occ: DateTime<Utc> = r.get("occurred_at");
        ensure!(
            occ == DateTime::parse_from_rfc3339("2026-08-09T09:00:00Z").unwrap(),
            "txn {id} occurred_at {occ} lost its override"
        );
        ensure!(
            r.get::<Option<String>, _>("edited_by").is_some(),
            "txn {id} lost its edit provenance"
        );
    }
    // The two stuck partials, now complete via the new templates.
    let partial_checks: &[(i64, &str, &str)] = if skip_floors() {
        &[]
    } else {
        &[(295_i64, "out", "135.72"), (305, "in", "5700.00")]
    };
    for &(id, dir, amount) in partial_checks {
        let r = sqlx::query(
            "SELECT direction, amount::text AS amount, currency
             FROM banksms.transactions WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
        let r = r.ok_or_else(|| AppError::Internal(format!("VERIFY: txn {id} missing")))?;
        ensure!(
            r.get::<String, _>("direction") == dir,
            "txn {id} direction wrong"
        );
        let got: String = r.get("amount");
        ensure!(
            Decimal::from_str(&got).unwrap() == Decimal::from_str(amount).unwrap(),
            "txn {id} amount {got} != {amount}"
        );
        ensure!(
            r.get::<String, _>("currency") == "EGP",
            "txn {id} currency wrong"
        );
    }

    // Loan links match whatever postings existed at cutover time.
    let linked: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM banksms.transactions WHERE loan_id IS NOT NULL")
            .fetch_one(&mut **tx)
            .await?;
    ensure!(
        linked == txns.legacy_postings,
        "loan links {linked} != legacy postings {}",
        txns.legacy_postings
    );

    // Fresh inserts get ids beyond every copied row.
    let probe: i64 = sqlx::query_scalar(
        "INSERT INTO banksms.raw_messages
             (wa_message_id, chat_jid, wa_timestamp, body, status)
         VALUES ('__CUTOVER_PROBE__', 'probe', now(), 'probe', 'ignored')
         RETURNING id",
    )
    .fetch_one(&mut **tx)
    .await?;
    let max_copied: i64 =
        sqlx::query_scalar("SELECT MAX(id) FROM banksms.raw_messages WHERE id <> $1")
            .bind(probe)
            .fetch_one(&mut **tx)
            .await?;
    ensure!(
        probe > max_copied,
        "sequence probe {probe} <= max copied id {max_copied}"
    );
    sqlx::query("DELETE FROM banksms.raw_messages WHERE id = $1")
        .bind(probe)
        .execute(&mut **tx)
        .await?;

    log::info!(
        "cutover verify OK: {new_live} live txns Σ {new_sum} · {new_raws} raws \
         ({} matched / {} suppressed / {} ignored) · {} loan link(s)",
        raws.matched,
        raws.suppressed,
        raws.ignored,
        linked
    );
    Ok(())
}
