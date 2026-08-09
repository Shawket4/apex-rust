//! Transaction endpoints.

use actix_web::{web, HttpRequest, HttpResponse};
use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};

use crate::api::{clamp_limit, if_match_version, require_read, require_write};
use crate::auth::JwtAuth;
use crate::errors::{AppError, AppResult};
use crate::parser::derive_fee;

/// Fields a user may override. Anything outside this list is rejected, so a
/// typo cannot silently create a phantom override that never surfaces.
const OVERRIDABLE: &[&str] = &[
    "direction",
    "amount",
    "currency",
    "account",
    "counterparty",
    "reference",
    "occurred_at",
];

#[derive(Debug, Serialize)]
pub struct ParsedView {
    pub direction: Option<String>,
    pub amount: Option<Decimal>,
    pub currency: Option<String>,
    pub account: Option<String>,
    pub counterparty: Option<String>,
    pub reference: Option<String>,
    pub balance_after: Option<Decimal>,
    pub occurred_at: Option<DateTime<Utc>>,
    pub template: Option<String>,
    pub parser_version: Option<i32>,
}

/// A transaction as the API returns it: effective values at the top level, with
/// the parser's original readings nested under `parsed`.
#[derive(Debug, Serialize)]
pub struct TransactionView {
    pub id: i64,
    pub version: i32,
    pub source: String,
    pub raw_message_id: Option<i64>,

    pub direction: Option<String>,
    pub amount: Option<Decimal>,
    pub currency: Option<String>,
    pub account: Option<String>,
    pub counterparty: Option<String>,
    pub reference: Option<String>,
    pub occurred_at: Option<DateTime<Utc>>,

    pub confidence: i16,
    pub parse_method: Option<String>,
    pub category: Option<String>,
    pub verified: bool,

    pub description: Option<String>,
    pub payment_method: Option<String>,
    pub company: Option<String>,
    pub car_no_plate: Option<String>,
    pub paid_by: Option<String>,

    /// Derived, never stored -- see parser::derive_fee.
    pub principal: Option<Decimal>,
    pub fee: Option<Decimal>,

    pub has_overrides: bool,
    /// False for fuel events and loans: they are read here but owned by the
    /// PetroApp sync and FalconGo respectively, so the UI must not offer edits.
    pub editable: bool,
    pub parsed: ParsedView,

    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The effective-value projection, shared by list and detail reads.
///
/// `COALESCE(override, parsed_*)` is applied in SQL rather than in Rust so that
/// filtering and sorting operate on the same values the client sees. Filtering on
/// the parsed value while displaying the override would be a subtle and very
/// confusing bug.
const SELECT_EFFECTIVE: &str = r#"
    SELECT t.id, t.version, t.source::text AS source, t.raw_message_id,
           COALESCE(o.direction, t.parsed_direction::text)            AS eff_direction,
           COALESCE(o.amount::numeric, t.parsed_amount)               AS eff_amount,
           COALESCE(o.currency, t.parsed_currency)                    AS eff_currency,
           COALESCE(o.account, t.parsed_account)                      AS eff_account,
           COALESCE(o.counterparty, t.parsed_counterparty)            AS eff_counterparty,
           COALESCE(o.reference, t.parsed_reference)                  AS eff_reference,
           COALESCE(o.occurred_at::timestamptz, t.parsed_occurred_at) AS eff_occurred_at,
           t.parsed_direction::text AS parsed_direction, t.parsed_amount, t.parsed_currency,
           t.parsed_account, t.parsed_counterparty, t.parsed_reference,
           t.parsed_balance_after, t.parsed_occurred_at, t.parsed_template, t.parser_version,
           t.confidence, t.parse_method::text AS parse_method,
           t.category, t.verified,
           t.description, t.payment_method, t.company, t.car_no_plate, t.paid_by,
           t.created_by, t.created_at, t.updated_at,
           (o.transaction_id IS NOT NULL) AS has_overrides
    FROM banksms.transactions t
    LEFT JOIN LATERAL (
        SELECT ov.transaction_id,
               MAX(CASE WHEN ov.field = 'direction'    THEN ov.value END) AS direction,
               MAX(CASE WHEN ov.field = 'amount'       THEN ov.value END) AS amount,
               MAX(CASE WHEN ov.field = 'currency'     THEN ov.value END) AS currency,
               MAX(CASE WHEN ov.field = 'account'      THEN ov.value END) AS account,
               MAX(CASE WHEN ov.field = 'counterparty' THEN ov.value END) AS counterparty,
               MAX(CASE WHEN ov.field = 'reference'    THEN ov.value END) AS reference,
               MAX(CASE WHEN ov.field = 'occurred_at'  THEN ov.value END) AS occurred_at
        FROM banksms.transaction_overrides ov
        WHERE ov.transaction_id = t.id AND ov.superseded_at IS NULL AND NOT ov.is_cleared
        GROUP BY ov.transaction_id
    ) o ON TRUE
"#;

fn row_to_view(r: &sqlx::postgres::PgRow) -> TransactionView {
    let amount: Option<Decimal> = r.get("eff_amount");
    let (principal, fee) = match amount {
        Some(a) => {
            let (p, f) = derive_fee(a);
            (Some(p), Some(f))
        }
        None => (None, None),
    };

    TransactionView {
        id: r.get("id"),
        version: r.get("version"),
        source: r.get("source"),
        raw_message_id: r.get("raw_message_id"),
        direction: r.get("eff_direction"),
        amount,
        currency: r.get("eff_currency"),
        account: r.get("eff_account"),
        counterparty: r.get("eff_counterparty"),
        reference: r.get("eff_reference"),
        occurred_at: r.get("eff_occurred_at"),
        confidence: r.get("confidence"),
        parse_method: r.get("parse_method"),
        category: r.get("category"),
        verified: r.get("verified"),
        description: r.get("description"),
        payment_method: r.get("payment_method"),
        company: r.get("company"),
        car_no_plate: r.get("car_no_plate"),
        paid_by: r.get("paid_by"),
        principal,
        fee,
        has_overrides: r.try_get("has_overrides").unwrap_or(false),
        editable: r.try_get("editable").unwrap_or(true),
        parsed: ParsedView {
            direction: r.get("parsed_direction"),
            amount: r.get("parsed_amount"),
            currency: r.get("parsed_currency"),
            account: r.get("parsed_account"),
            counterparty: r.get("parsed_counterparty"),
            reference: r.get("parsed_reference"),
            balance_after: r.get("parsed_balance_after"),
            occurred_at: r.get("parsed_occurred_at"),
            template: r.get("parsed_template"),
            parser_version: r.get("parser_version"),
        },
        created_by: r.get("created_by"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default, deserialize_with = "crate::api::flexible_date::start")]
    pub from: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "crate::api::flexible_date::end")]
    pub to: Option<DateTime<Utc>>,
    pub account: Option<String>,
    pub direction: Option<String>,
    pub min_amount: Option<Decimal>,
    pub max_amount: Option<Decimal>,
    pub counterparty: Option<String>,
    pub template: Option<String>,
    pub source: Option<String>,
    pub category: Option<String>,
    pub company: Option<String>,
    pub car_no_plate: Option<String>,
    pub payment_method: Option<String>,
    pub verified: Option<bool>,
    pub q: Option<String>,
    /// Opaque cursor: "<occurred_at_millis>:<id>" from the previous page.
    /// A composite is required because the list unions three sources whose ids
    /// come from independent sequences.
    pub cursor: Option<String>,
    pub limit: Option<i64>,

    /// Include read-only fuel events / driver loans, matching the legacy costs
    /// view. Default true; pass "false" to get the bank-SMS ledger alone.
    pub include_fuel: Option<String>,
    pub include_loans: Option<String>,
}

fn flag(v: &Option<String>) -> bool {
    v.as_deref().map_or(true, |s| s != "false" && s != "0")
}

impl ListQuery {
    fn to_unified(&self) -> crate::api::unified::UnifiedFilters {
        crate::api::unified::UnifiedFilters {
            from: self.from,
            to: self.to,
            category: self.category.clone(),
            company: self.company.clone(),
            car_no_plate: self.car_no_plate.clone(),
            payment_method: self.payment_method.clone(),
            source: self.source.clone(),
            account: self.account.clone(),
            direction: self.direction.clone(),
            template: self.template.clone(),
            counterparty: self.counterparty.clone(),
            min_amount: self.min_amount,
            max_amount: self.max_amount,
            verified: self.verified,
            q: self.q.clone(),
            include_fuel: flag(&self.include_fuel),
            include_loans: flag(&self.include_loans),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Page<T> {
    pub data: Vec<T>,
    /// Absent when there are no further pages. Opaque composite token —
    /// "<occurred_at_millis>:<id>" — because the list unions three sources whose
    /// ids come from independent sequences.
    pub next_cursor: Option<String>,
}

/// Cursor pagination, not offset: the ledger is append-heavy, and with offset
/// paging a row inserted mid-scan silently shifts every subsequent page.
async fn list(
    pool: web::Data<PgPool>,
    q: web::Query<ListQuery>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    require_read(&req)?;
    let limit = clamp_limit(q.limit);
    let filters = q.to_unified();

    // No branch can satisfy these filters -- an empty page, not an error.
    let Some(union) = crate::api::unified::build_union(&filters) else {
        return Ok(HttpResponse::Ok().json(Page::<TransactionView> {
            data: vec![],
            next_cursor: None,
        }));
    };

    // Composite cursor. Ordering by occurred_at alone is not stable (many rows
    // share a day, and the synthesised fuel/loan rows all land at local
    // midnight), so id is the tiebreaker and both travel in the cursor.
    let (cursor_ts, cursor_id) = parse_cursor(q.cursor.as_deref());

    let sql = format!(
        "SELECT * FROM ({union}) u
         WHERE ($16::timestamptz IS NULL
                OR (u.eff_occurred_at, u.id) < ($16::timestamptz, $17::bigint))
         ORDER BY u.eff_occurred_at DESC NULLS LAST, u.id DESC
         LIMIT $18"
    );

    let rows = sqlx::query(&sql)
        .bind(filters.from)              // $1
        .bind(filters.to)                // $2
        .bind(&filters.category)         // $3
        .bind(&filters.company)          // $4
        .bind(&filters.car_no_plate)     // $5
        .bind(&filters.payment_method)   // $6
        .bind(&filters.source)           // $7
        .bind(&filters.account)          // $8
        .bind(&filters.direction)        // $9
        .bind(&filters.template)         // $10
        .bind(filters.min_amount)        // $11
        .bind(filters.max_amount)        // $12
        .bind(filters.verified)          // $13
        .bind(&filters.counterparty)     // $14
        .bind(&filters.q)                // $15
        .bind(cursor_ts)                 // $16
        .bind(cursor_id)                 // $17
        .bind(limit)                     // $18
        .fetch_all(pool.get_ref())
        .await?;

    let data: Vec<TransactionView> = rows.iter().map(row_to_view).collect();

    let next_cursor = if data.len() as i64 == limit {
        data.last().and_then(|t| {
            t.occurred_at.map(|ts| format!("{}:{}", ts.timestamp_millis(), t.id))
        })
    } else {
        None
    };

    Ok(HttpResponse::Ok().json(Page { data, next_cursor }))
}

/// Decode "<millis>:<id>". A malformed cursor starts from the beginning rather
/// than erroring -- it is an opaque token the client should not be building by
/// hand, and failing the whole request over it helps nobody.
fn parse_cursor(raw: Option<&str>) -> (Option<DateTime<Utc>>, Option<i64>) {
    let Some(raw) = raw.filter(|s| !s.is_empty()) else {
        return (None, None);
    };
    let Some((ts, id)) = raw.split_once(':') else {
        return (None, None);
    };
    match (ts.parse::<i64>(), id.parse::<i64>()) {
        (Ok(ms), Ok(id)) => (DateTime::from_timestamp_millis(ms), Some(id)),
        _ => (None, None),
    }
}

async fn get_one(
    pool: web::Data<PgPool>,
    path: web::Path<i64>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    require_read(&req)?;
    let id = path.into_inner();

    let sql = format!("{SELECT_EFFECTIVE} WHERE t.id = $1 AND t.deleted_at IS NULL");
    let row = sqlx::query(&sql)
        .bind(id)
        .fetch_optional(pool.get_ref())
        .await?
        .ok_or_else(|| AppError::NotFound(format!("transaction {id}")))?;

    Ok(HttpResponse::Ok().json(row_to_view(&row)))
}

#[derive(Debug, Deserialize)]
pub struct CreateTransaction {
    pub direction: String,
    pub amount: Decimal,
    pub currency: String,
    pub occurred_at: DateTime<Utc>,
    pub account: Option<String>,
    pub counterparty: Option<String>,
    pub reference: Option<String>,
    pub category: Option<String>,
    pub description: Option<String>,
    pub payment_method: Option<String>,
    pub company: Option<String>,
    pub car_no_plate: Option<String>,
    pub paid_by: Option<String>,
}

/// Validate human input. The parser guarantees shape; humans do not.
fn validate(c: &CreateTransaction) -> AppResult<()> {
    if c.amount <= Decimal::ZERO {
        return Err(AppError::BadRequest("amount must be greater than zero".into()));
    }
    if !matches!(c.direction.as_str(), "in" | "out") {
        return Err(AppError::BadRequest("direction must be 'in' or 'out'".into()));
    }
    // ISO 4217 is three uppercase letters. Not a currency-table lookup, but it
    // rejects the realistic mistakes ("EGP " , "egp", "pounds").
    if c.currency.len() != 3 || !c.currency.chars().all(|ch| ch.is_ascii_uppercase()) {
        return Err(AppError::BadRequest(
            "currency must be a 3-letter uppercase ISO-4217 code".into(),
        ));
    }
    if c.occurred_at > Utc::now() + Duration::hours(24) {
        return Err(AppError::BadRequest(
            "occurred_at cannot be more than 24 hours in the future".into(),
        ));
    }
    for (name, value) in [
        ("counterparty", &c.counterparty),
        ("reference", &c.reference),
        ("category", &c.category),
        ("description", &c.description),
        ("company", &c.company),
        ("car_no_plate", &c.car_no_plate),
        ("paid_by", &c.paid_by),
    ] {
        if let Some(v) = value {
            if v.chars().count() > 500 {
                return Err(AppError::BadRequest(format!("{name} is too long")));
            }
            if v.chars().any(|ch| ch.is_control()) {
                return Err(AppError::BadRequest(format!(
                    "{name} must not contain control characters"
                )));
            }
        }
    }
    Ok(())
}

/// Manual create.
///
/// The row's values go into the `parsed_*` columns with parse_method='manual'
/// and parser_version=0. That is safe because every reparse path is additionally
/// scoped to `source = 'whatsapp'`, so a manual row is never rebuilt -- which is
/// exactly the guarantee the acceptance test checks.
async fn create(
    pool: web::Data<PgPool>,
    body: web::Json<CreateTransaction>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    let ctx = require_write(&req)?;
    let c = body.into_inner();
    validate(&c)?;

    let id: i64 = sqlx::query_scalar(
        r#"
        INSERT INTO banksms.transactions (
            source, parsed_direction, parsed_amount, parsed_currency, parsed_occurred_at,
            parsed_account, parsed_counterparty, parsed_reference,
            parse_method, parser_version, confidence,
            category, description, payment_method, company, car_no_plate, paid_by,
            verified, created_by
        ) VALUES (
            'manual', $1::text::banksms.direction, $2, $3, $4,
            $5, $6, $7,
            'manual', 0, 100,
            $8, $9, $10, $11, $12, $13,
            true, $14
        )
        RETURNING id
        "#,
    )
    .bind(&c.direction)
    .bind(c.amount)
    .bind(&c.currency)
    .bind(c.occurred_at)
    .bind(&c.account)
    .bind(&c.counterparty)
    .bind(&c.reference)
    .bind(&c.category)
    .bind(&c.description)
    .bind(&c.payment_method)
    .bind(&c.company)
    .bind(&c.car_no_plate)
    .bind(&c.paid_by)
    .bind(ctx.actor())
    .fetch_one(pool.get_ref())
    .await?;

    let sql = format!("{SELECT_EFFECTIVE} WHERE t.id = $1");
    let row = sqlx::query(&sql).bind(id).fetch_one(pool.get_ref()).await?;
    Ok(HttpResponse::Created().json(row_to_view(&row)))
}

#[derive(Debug, Deserialize)]
pub struct PatchTransaction {
    // Overridable parser-owned fields.
    pub direction: Option<String>,
    pub amount: Option<Decimal>,
    pub currency: Option<String>,
    pub account: Option<String>,
    pub counterparty: Option<String>,
    pub reference: Option<String>,
    pub occurred_at: Option<DateTime<Utc>>,
    // Directly user-owned columns.
    pub category: Option<String>,
    pub verified: Option<bool>,
    pub description: Option<String>,
    pub payment_method: Option<String>,
    pub company: Option<String>,
    pub car_no_plate: Option<String>,
    pub paid_by: Option<String>,
}

/// Partial update with optimistic concurrency.
///
/// Corrections to parser-owned fields become rows in `transaction_overrides`
/// rather than in-place edits, so the original SMS reading is never lost and
/// `/history` has something real to show.
async fn patch(
    pool: web::Data<PgPool>,
    path: web::Path<i64>,
    body: web::Json<PatchTransaction>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    let ctx = require_write(&req)?;
    let id = path.into_inner();
    let expected = if_match_version(&req)?;
    let p = body.into_inner();

    let mut tx = pool.begin().await?;

    // Lock the row so the version check and the write cannot interleave.
    let current: Option<(i32, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT version, deleted_at FROM banksms.transactions WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;

    let (actual, deleted_at) =
        current.ok_or_else(|| AppError::NotFound(format!("transaction {id}")))?;
    if deleted_at.is_some() {
        return Err(AppError::NotFound(format!("transaction {id}")));
    }
    if actual != expected {
        return Err(AppError::VersionConflict { expected, actual });
    }

    let overrides: Vec<(&str, Option<String>)> = vec![
        ("direction", p.direction.clone()),
        ("amount", p.amount.map(|v| v.to_string())),
        ("currency", p.currency.clone()),
        ("account", p.account.clone()),
        ("counterparty", p.counterparty.clone()),
        ("reference", p.reference.clone()),
        ("occurred_at", p.occurred_at.map(|v| v.to_rfc3339())),
    ];

    for (field, value) in overrides.into_iter().filter(|(_, v)| v.is_some()) {
        debug_assert!(OVERRIDABLE.contains(&field));
        let value = value.unwrap();

        if field == "direction" && !matches!(value.as_str(), "in" | "out") {
            return Err(AppError::BadRequest("direction must be 'in' or 'out'".into()));
        }

        // Supersede rather than replace: the previous value is the audit trail.
        sqlx::query(
            "UPDATE banksms.transaction_overrides SET superseded_at = now() \
             WHERE transaction_id = $1 AND field = $2 AND superseded_at IS NULL",
        )
        .bind(id)
        .bind(field)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO banksms.transaction_overrides (transaction_id, field, value, actor) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(field)
        .bind(&value)
        .bind(ctx.actor())
        .execute(&mut *tx)
        .await?;
    }

    // User-owned columns are updated in place -- they were never the parser's.
    sqlx::query(
        r#"
        UPDATE banksms.transactions
        SET category       = COALESCE($2, category),
            verified       = COALESCE($3, verified),
            description    = COALESCE($4, description),
            payment_method = COALESCE($5, payment_method),
            company        = COALESCE($6, company),
            car_no_plate   = COALESCE($7, car_no_plate),
            paid_by        = COALESCE($8, paid_by),
            version        = version + 1,
            updated_at     = now()
        WHERE id = $1
        "#,
    )
    .bind(id)
    .bind(&p.category)
    .bind(p.verified)
    .bind(&p.description)
    .bind(&p.payment_method)
    .bind(&p.company)
    .bind(&p.car_no_plate)
    .bind(&p.paid_by)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    let sql = format!("{SELECT_EFFECTIVE} WHERE t.id = $1");
    let row = sqlx::query(&sql).bind(id).fetch_one(pool.get_ref()).await?;
    Ok(HttpResponse::Ok().json(row_to_view(&row)))
}

/// Soft delete. Parsed rows are never hard-deleted: the raw message still exists,
/// so a hard delete would simply be re-materialised by the next reparse.
async fn soft_delete(
    pool: web::Data<PgPool>,
    path: web::Path<i64>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    require_write(&req)?;
    let id = path.into_inner();
    let expected = if_match_version(&req)?;

    let result = sqlx::query(
        "UPDATE banksms.transactions SET deleted_at = now(), version = version + 1, \
         updated_at = now() WHERE id = $1 AND version = $2 AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(expected)
    .execute(pool.get_ref())
    .await?;

    if result.rows_affected() == 0 {
        let actual: Option<i32> =
            sqlx::query_scalar("SELECT version FROM banksms.transactions WHERE id = $1")
                .bind(id)
                .fetch_optional(pool.get_ref())
                .await?;
        return match actual {
            Some(actual) => Err(AppError::VersionConflict { expected, actual }),
            None => Err(AppError::NotFound(format!("transaction {id}"))),
        };
    }

    Ok(HttpResponse::NoContent().finish())
}

#[derive(Debug, Serialize)]
pub struct HistoryEntry {
    pub field: String,
    pub value: Option<String>,
    pub is_cleared: bool,
    pub actor: Option<String>,
    pub set_at: DateTime<Utc>,
    pub superseded_at: Option<DateTime<Utc>>,
    pub is_current: bool,
}

async fn history(
    pool: web::Data<PgPool>,
    path: web::Path<i64>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    require_read(&req)?;
    let id = path.into_inner();

    let rows = sqlx::query(
        "SELECT field, value, is_cleared, actor, set_at, superseded_at \
         FROM banksms.transaction_overrides WHERE transaction_id = $1 ORDER BY set_at DESC",
    )
    .bind(id)
    .fetch_all(pool.get_ref())
    .await?;

    let entries: Vec<HistoryEntry> = rows
        .iter()
        .map(|r| {
            let superseded_at: Option<DateTime<Utc>> = r.get("superseded_at");
            HistoryEntry {
                field: r.get("field"),
                value: r.get("value"),
                is_cleared: r.get("is_cleared"),
                actor: r.get("actor"),
                set_at: r.get("set_at"),
                superseded_at,
                is_current: superseded_at.is_none(),
            }
        })
        .collect();

    Ok(HttpResponse::Ok().json(entries))
}

#[derive(Debug, Serialize)]
pub struct AccountRollup {
    pub account: Option<String>,
    pub transactions: i64,
    pub total_in: Decimal,
    pub total_out: Decimal,
    pub last_seen: Option<DateTime<Utc>>,
}

async fn accounts(pool: web::Data<PgPool>, req: HttpRequest) -> AppResult<HttpResponse> {
    require_read(&req)?;

    let rows = sqlx::query(
        r#"
        SELECT parsed_account AS account,
               count(*) AS transactions,
               COALESCE(SUM(parsed_amount) FILTER (WHERE parsed_direction = 'in'), 0)  AS total_in,
               COALESCE(SUM(parsed_amount) FILTER (WHERE parsed_direction = 'out'), 0) AS total_out,
               MAX(parsed_occurred_at) AS last_seen
        FROM banksms.transactions
        WHERE deleted_at IS NULL
        GROUP BY parsed_account
        ORDER BY count(*) DESC
        "#,
    )
    .fetch_all(pool.get_ref())
    .await?;

    let data: Vec<AccountRollup> = rows
        .iter()
        .map(|r| AccountRollup {
            account: r.get("account"),
            transactions: r.get("transactions"),
            total_in: r.get("total_in"),
            total_out: r.get("total_out"),
            last_seen: r.get("last_seen"),
        })
        .collect();

    Ok(HttpResponse::Ok().json(data))
}

#[derive(Debug, Deserialize)]
pub struct SummaryQuery {
    #[serde(default, deserialize_with = "crate::api::flexible_date::start")]
    pub from: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "crate::api::flexible_date::end")]
    pub to: Option<DateTime<Utc>>,
    /// day | account | counterparty | category | company
    pub group_by: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SummaryBucket {
    pub key: Option<String>,
    pub transactions: i64,
    pub total_in: Decimal,
    pub total_out: Decimal,
    pub net: Decimal,
    /// Sum of derived fees across the bucket. Derived per row, never stored.
    pub fees: Decimal,
}

async fn summary(
    pool: web::Data<PgPool>,
    q: web::Query<SummaryQuery>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    require_read(&req)?;

    // Whitelisted, never interpolated from raw input.
    let group_expr = match q.group_by.as_deref() {
        None | Some("day") => "to_char(date_trunc('day', parsed_occurred_at AT TIME ZONE 'Africa/Cairo'), 'YYYY-MM-DD')",
        Some("account") => "parsed_account",
        Some("counterparty") => "parsed_counterparty",
        Some("category") => "category",
        Some("company") => "company",
        Some(other) => {
            return Err(AppError::BadRequest(format!(
                "group_by must be one of day, account, counterparty, category, company (got {other:?})"
            )))
        }
    };

    let sql = format!(
        r#"
        SELECT {group_expr} AS key,
               id, parsed_direction::text AS direction, parsed_amount AS amount
        FROM banksms.transactions
        WHERE deleted_at IS NULL
          AND ($1::timestamptz IS NULL OR parsed_occurred_at >= $1)
          AND ($2::timestamptz IS NULL OR parsed_occurred_at <= $2)
        "#
    );

    let rows = sqlx::query(&sql)
        .bind(q.from)
        .bind(q.to)
        .fetch_all(pool.get_ref())
        .await?;

    // Fees are derived per row, so the aggregation happens here rather than in
    // SQL -- summing a derived value in the database would mean storing it.
    use std::collections::BTreeMap;
    let mut buckets: BTreeMap<Option<String>, SummaryBucket> = BTreeMap::new();

    for r in &rows {
        let key: Option<String> = r.try_get("key").unwrap_or(None);
        let direction: Option<String> = r.get("direction");
        let amount: Option<Decimal> = r.get("amount");

        let b = buckets.entry(key.clone()).or_insert_with(|| SummaryBucket {
            key,
            transactions: 0,
            total_in: Decimal::ZERO,
            total_out: Decimal::ZERO,
            net: Decimal::ZERO,
            fees: Decimal::ZERO,
        });

        b.transactions += 1;
        if let Some(a) = amount {
            let (_principal, fee) = derive_fee(a);
            b.fees += fee;
            match direction.as_deref() {
                Some("in") => b.total_in += a,
                _ => b.total_out += a,
            }
        }
    }

    let mut data: Vec<SummaryBucket> = buckets.into_values().collect();
    for b in &mut data {
        b.net = b.total_in - b.total_out;
    }

    Ok(HttpResponse::Ok().json(data))
}

#[derive(Debug, Deserialize)]
pub struct StatisticsQuery {
    #[serde(default, deserialize_with = "crate::api::flexible_date::start")]
    pub from: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "crate::api::flexible_date::end")]
    pub to: Option<DateTime<Utc>>,
    pub category: Option<String>,
    pub company: Option<String>,
    pub car_no_plate: Option<String>,
    pub payment_method: Option<String>,
    pub source: Option<String>,
    pub account: Option<String>,
    pub include_fuel: Option<String>,
    pub include_loans: Option<String>,
}

impl StatisticsQuery {
    fn to_unified(&self) -> crate::api::unified::UnifiedFilters {
        crate::api::unified::UnifiedFilters {
            from: self.from,
            to: self.to,
            category: self.category.clone(),
            company: self.company.clone(),
            car_no_plate: self.car_no_plate.clone(),
            payment_method: self.payment_method.clone(),
            source: self.source.clone(),
            account: self.account.clone(),
            include_fuel: flag(&self.include_fuel),
            include_loans: flag(&self.include_loans),
            ..Default::default()
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Breakdown {
    pub key: String,
    pub count: i64,
    pub total_amount: Decimal,
}

/// The aggregate the expenses dashboard renders in one request.
///
/// Every figure comes from `banksms.transactions`. `public.fleet_expenses` is
/// never read: its rows live here as `source = 'import'`.
#[derive(Debug, Serialize)]
pub struct Statistics {
    pub total_amount: Decimal,
    pub total_in: Decimal,
    pub total_out: Decimal,
    pub net: Decimal,
    /// Derived per row, then summed. Never a stored column.
    pub total_fees: Decimal,
    pub expense_count: i64,
    pub by_date: Vec<Breakdown>,
    pub by_type: Vec<Breakdown>,
    pub by_car: Vec<Breakdown>,
    pub by_company: Vec<Breakdown>,
    pub by_source: Vec<Breakdown>,
    pub by_payment_method: Vec<Breakdown>,
    pub by_account: Vec<Breakdown>,
}

impl Statistics {
    fn empty() -> Self {
        Self {
            total_amount: Decimal::ZERO, total_in: Decimal::ZERO,
            total_out: Decimal::ZERO, net: Decimal::ZERO,
            total_fees: Decimal::ZERO, expense_count: 0,
            by_date: vec![], by_type: vec![], by_car: vec![], by_company: vec![],
            by_source: vec![], by_payment_method: vec![], by_account: vec![],
        }
    }
}

/// One pass over the filtered rows, bucketed in Rust.
///
/// Deliberately not seven SQL aggregates: the fee is a derived value (see
/// parser::derive_fee), so it has to be computed per row anyway, and doing the
/// grouping here keeps every breakdown consistent with a single snapshot of the
/// data rather than seven separately-timed queries.
async fn statistics(
    pool: web::Data<PgPool>,
    q: web::Query<StatisticsQuery>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    require_read(&req)?;

    let filters = q.to_unified();
    let Some(union) = crate::api::unified::build_union(&filters) else {
        return Ok(HttpResponse::Ok().json(Statistics::empty()));
    };

    // Same union as the list, so the totals can never disagree with the rows
    // they claim to summarise.
    let sql = format!(
        "SELECT
            to_char(date_trunc('day', u.eff_occurred_at AT TIME ZONE 'Africa/Cairo'),
                    'YYYY-MM-DD')  AS day,
            u.category, u.car_no_plate, u.company, u.payment_method,
            u.source, u.eff_account AS account,
            u.eff_direction AS direction, u.eff_amount AS amount
         FROM ({union}) u"
    );

    let rows = sqlx::query(&sql)
        .bind(filters.from)
        .bind(filters.to)
        .bind(&filters.category)
        .bind(&filters.company)
        .bind(&filters.car_no_plate)
        .bind(&filters.payment_method)
        .bind(&filters.source)
        .bind(&filters.account)
        .bind(&filters.direction)
        .bind(&filters.template)
        .bind(filters.min_amount)
        .bind(filters.max_amount)
        .bind(filters.verified)
        .bind(&filters.counterparty)
        .bind(&filters.q)
        .fetch_all(pool.get_ref())
        .await?;

    use std::collections::HashMap;
    let mut totals = (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO); // in, out, fees
    let mut buckets: HashMap<&'static str, HashMap<String, (i64, Decimal)>> = HashMap::new();

    for r in &rows {
        let amount: Decimal = r.get::<Option<Decimal>, _>("amount").unwrap_or(Decimal::ZERO);
        let direction: Option<String> = r.get("direction");

        let (_principal, fee) = derive_fee(amount);
        totals.2 += fee;
        match direction.as_deref() {
            Some("in") => totals.0 += amount,
            _ => totals.1 += amount,
        }

        // "(none)" rather than dropping the row: an expense with no company is
        // still money, and silently omitting it would make the breakdowns fail
        // to sum to the total.
        let dims: [(&'static str, Option<String>); 7] = [
            ("date", r.get::<Option<String>, _>("day")),
            ("type", r.get::<Option<String>, _>("category")),
            ("car", r.get::<Option<String>, _>("car_no_plate")),
            ("company", r.get::<Option<String>, _>("company")),
            ("source", r.get::<Option<String>, _>("source")),
            ("payment_method", r.get::<Option<String>, _>("payment_method")),
            ("account", r.get::<Option<String>, _>("account")),
        ];

        for (dim, value) in dims {
            let key = value.filter(|v| !v.is_empty()).unwrap_or_else(|| "(none)".to_string());
            let entry = buckets.entry(dim).or_default().entry(key).or_insert((0, Decimal::ZERO));
            entry.0 += 1;
            entry.1 += amount;
        }
    }

    // Dates ascending (they are a time series); everything else by value
    // descending, because the dashboard shows "top N".
    let take = |dim: &str, chronological: bool| -> Vec<Breakdown> {
        let mut v: Vec<Breakdown> = buckets
            .get(dim)
            .map(|m| {
                m.iter()
                    .map(|(key, (count, total))| Breakdown {
                        key: key.clone(),
                        count: *count,
                        total_amount: *total,
                    })
                    .collect()
            })
            .unwrap_or_default();
        if chronological {
            v.sort_by(|a, b| a.key.cmp(&b.key));
        } else {
            v.sort_by(|a, b| b.total_amount.cmp(&a.total_amount));
        }
        v
    };

    Ok(HttpResponse::Ok().json(Statistics {
        total_amount: totals.0 + totals.1,
        total_in: totals.0,
        total_out: totals.1,
        net: totals.0 - totals.1,
        total_fees: totals.2,
        expense_count: rows.len() as i64,
        by_date: take("date", true),
        by_type: take("type", false),
        by_car: take("car", false),
        by_company: take("company", false),
        by_source: take("source", false),
        by_payment_method: take("payment_method", false),
        by_account: take("account", false),
    }))
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    let guard = || JwtAuth { required_permission: None };
    cfg.service(
        web::scope("/api/v1/transactions")
            .route("", web::get().to(list).wrap(guard()))
            .route("", web::post().to(create).wrap(guard()))
            // Registered before /{id} so "statistics" is not captured as an id.
            .route("/statistics", web::get().to(statistics).wrap(guard()))
            .route("/{id}", web::get().to(get_one).wrap(guard()))
            .route("/{id}", web::patch().to(patch).wrap(guard()))
            .route("/{id}", web::put().to(patch).wrap(guard()))
            .route("/{id}", web::delete().to(soft_delete).wrap(guard()))
            .route("/{id}/history", web::get().to(history).wrap(guard()))
            // Nested note and tag routes live here rather than in their own
            // module's configure(): actix matches the FIRST scope whose prefix
            // matches and 404s inside it, so a second scope on the same prefix
            // would be unreachable.
            .route("/{id}/notes", web::get().to(crate::api::notes_tags::list_notes).wrap(guard()))
            .route("/{id}/notes", web::post().to(crate::api::notes_tags::create_note).wrap(guard()))
            .route("/{id}/tags", web::post().to(crate::api::notes_tags::attach_tag).wrap(guard()))
            .route("/{id}/tags/{tag_id}", web::delete().to(crate::api::notes_tags::detach_tag).wrap(guard())),
    )
    .route("/api/v1/accounts", web::get().to(accounts).wrap(guard()))
    .route("/api/v1/summary", web::get().to(summary).wrap(guard()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn valid() -> CreateTransaction {
        CreateTransaction {
            direction: "out".into(),
            amount: dec!(100.00),
            currency: "EGP".into(),
            occurred_at: Utc::now(),
            account: None,
            counterparty: Some("Test".into()),
            reference: None,
            category: None,
            description: None,
            payment_method: None,
            company: None,
            car_no_plate: None,
            paid_by: None,
        }
    }

    #[test]
    fn accepts_a_well_formed_manual_transaction() {
        assert!(validate(&valid()).is_ok());
    }

    #[test]
    fn rejects_non_positive_amounts() {
        let mut c = valid();
        c.amount = dec!(0);
        assert!(validate(&c).is_err());
        c.amount = dec!(-5);
        assert!(validate(&c).is_err());
    }

    #[test]
    fn rejects_bad_direction() {
        let mut c = valid();
        c.direction = "sideways".into();
        assert!(validate(&c).is_err());
    }

    #[test]
    fn rejects_non_iso_currency() {
        for bad in ["egp", "EGPP", "EG", "£", "pounds"] {
            let mut c = valid();
            c.currency = bad.into();
            assert!(validate(&c).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn rejects_dates_far_in_the_future() {
        let mut c = valid();
        c.occurred_at = Utc::now() + Duration::days(2);
        assert!(validate(&c).is_err());

        // Just inside the 24h allowance: clock skew and timezone confusion are
        // normal, so this must be accepted.
        c.occurred_at = Utc::now() + Duration::hours(12);
        assert!(validate(&c).is_ok());
    }

    #[test]
    fn rejects_control_characters_and_overlong_text() {
        let mut c = valid();
        c.counterparty = Some("bad\u{0}name".into());
        assert!(validate(&c).is_err());

        let mut c = valid();
        c.description = Some("x".repeat(501));
        assert!(validate(&c).is_err());
    }

    #[test]
    fn every_patchable_parser_field_is_in_the_override_allowlist() {
        // Guards against adding a field to PatchTransaction and forgetting the
        // allow-list, which would create an override that never reads back.
        for field in [
            "direction", "amount", "currency", "account", "counterparty", "reference",
            "occurred_at",
        ] {
            assert!(OVERRIDABLE.contains(&field), "{field} missing from allow-list");
        }
    }
}
