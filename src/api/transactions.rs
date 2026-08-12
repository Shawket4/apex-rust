//! Transactions: list (bank + optional fuel/loan blending), statistics,
//! server-side XLSX export, and CRUD with If-Match optimistic concurrency.
//!
//! Dynamic SQL goes through `QueryBuilder` exclusively. Two production
//! outages came from hand-numbered `$N` parameter lists; there are none here.

use actix_web::{web, HttpRequest, HttpResponse};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use std::collections::HashMap;
use std::str::FromStr;

use super::registration::{self, LoanInfo, Registrable};
use crate::errors::{AppError, AppResult};
use crate::parser::derive_fee;

pub const FUEL_ID_BASE: i64 = 1_000_000_000_000;
pub const LOAN_ID_BASE: i64 = 2_000_000_000_000;

/* ------------------------------------------------------------------------ */
/* Wire types                                                                */
/* ------------------------------------------------------------------------ */

#[derive(Debug, Serialize)]
pub struct TransactionView {
    pub id: i64,
    pub source: String,
    pub raw_message_id: Option<i64>,
    pub direction: String,
    pub amount: Decimal, // serde-with-str: crosses the wire as a string
    pub currency: String,
    pub occurred_at: DateTime<Utc>,
    pub account: Option<String>,
    pub counterparty: Option<String>,
    pub reference: Option<String>,
    pub category: Option<String>,
    pub description: Option<String>,
    pub payment_method: Option<String>,
    pub company: Option<String>,
    pub car_id: Option<i64>,
    pub car_no_plate: Option<String>,
    pub driver_id: Option<i64>,
    pub employee_id: Option<i64>,
    pub paid_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loan: Option<LoanInfo>,
    pub principal: Option<Decimal>,
    pub fee: Option<Decimal>,
    pub version: i32,
    pub editable: bool,
    pub edited_by: Option<String>,
    pub edited_at: Option<DateTime<Utc>>,
    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct Page {
    pub data: Vec<TransactionView>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub category: Option<String>,
    pub company: Option<String>,
    pub payment_method: Option<String>,
    pub q: Option<String>,
    pub source: Option<String>,
    #[serde(default)]
    pub include_fuel: Option<String>,
    #[serde(default)]
    pub include_loans: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
    /// Export only: render the sheet right-to-left for Arabic locales.
    #[serde(default)]
    pub rtl: Option<String>,
}

fn flag(v: &Option<String>, default: bool) -> bool {
    match v.as_deref() {
        None => default,
        Some("false") | Some("0") => false,
        _ => true,
    }
}

impl ListQuery {
    fn wants_fuel(&self) -> bool {
        // A bank-only filter prunes the fuel branch outright.
        flag(&self.include_fuel, true)
            && self.source.is_none()
            && self.category.as_deref().map_or(true, |c| c == "Fuel")
    }
    fn wants_loans(&self) -> bool {
        flag(&self.include_loans, true)
            && self.source.is_none()
            && self.category.as_deref().map_or(true, |c| c == "Loan")
            && self.company.is_none()
    }
}

/* ------------------------------------------------------------------------ */
/* The union                                                                 */
/* ------------------------------------------------------------------------ */

/// Push the three-branch union as a parenthesised subquery aliased `u`.
/// Every branch projects the same column list; filters bind per branch.
fn push_union<'a>(qb: &mut QueryBuilder<'a, Postgres>, f: &'a ListQuery) {
    qb.push("(SELECT t.id, t.source, t.raw_message_id, TRUE AS editable, ");
    qb.push(
        "t.direction, t.amount, t.currency, t.occurred_at, t.account, t.counterparty, \
         t.reference, t.category, t.description, t.payment_method, t.company, \
         t.car_id, t.car_no_plate, t.driver_id, t.employee_id, t.loan_id, t.paid_by, \
         t.version, t.created_by, t.edited_by, t.edited_at, t.created_at, t.updated_at \
         FROM banksms.transactions t WHERE t.deleted_at IS NULL",
    );
    if let Some(from) = &f.from {
        qb.push(" AND t.occurred_at >= ").push_bind(from);
    }
    if let Some(to) = &f.to {
        qb.push(" AND t.occurred_at <= ").push_bind(to);
    }
    if let Some(c) = &f.category {
        if c == "__uncategorized__" {
            qb.push(" AND t.category IS NULL");
        } else {
            qb.push(" AND t.category = ").push_bind(c);
        }
    }
    if let Some(c) = &f.company {
        qb.push(" AND t.company = ").push_bind(c);
    }
    if let Some(p) = &f.payment_method {
        qb.push(" AND t.payment_method = ").push_bind(p);
    }
    if let Some(s) = &f.source {
        qb.push(" AND t.source = ").push_bind(s);
    }
    if let Some(q) = &f.q {
        qb.push(" AND (t.counterparty ILIKE '%' || ")
            .push_bind(q)
            .push(" || '%' OR t.description ILIKE '%' || ")
            .push_bind(q)
            .push(" || '%' OR t.reference ILIKE '%' || ")
            .push_bind(q)
            .push(" || '%' OR t.paid_by ILIKE '%' || ")
            .push_bind(q)
            .push(" || '%')");
    }

    if f.wants_fuel() {
        // Mapping mirrors the legacy costs view exactly so historical numbers
        // keep matching: driver as counterparty, transporter as company.
        qb.push(
            " UNION ALL SELECT (fe.id + 1000000000000)::bigint, 'fuel_event', NULL::bigint, FALSE, \
             'out', COALESCE(fe.price, 0)::numeric, 'EGP', \
             (fe.date::date::timestamp AT TIME ZONE 'Africa/Cairo'), NULL, fe.driver_name, \
             NULL, 'Fuel', CONCAT('Fuel: ', COALESCE(fe.liters::text, '0'), 'L @ ', \
             COALESCE(fe.price_per_liter::text, '0'), '/L'), COALESCE(fe.method, 'Cash'), \
             fe.transporter, NULL::bigint, fe.car_no_plate, NULL::bigint, NULL::bigint, \
             NULL::bigint, fe.driver_name, 1, NULL, NULL, NULL::timestamptz, \
             fe.created_at AT TIME ZONE 'UTC', fe.updated_at AT TIME ZONE 'UTC' \
             FROM public.fuel_events fe \
             WHERE fe.deleted_at IS NULL AND fe.date IS NOT NULL AND fe.created_at IS NOT NULL",
        );
        if let Some(from) = &f.from {
            qb.push(" AND (fe.date::date::timestamp AT TIME ZONE 'Africa/Cairo') >= ")
                .push_bind(from);
        }
        if let Some(to) = &f.to {
            qb.push(" AND (fe.date::date::timestamp AT TIME ZONE 'Africa/Cairo') <= ")
                .push_bind(to);
        }
        if let Some(c) = &f.company {
            qb.push(" AND fe.transporter = ").push_bind(c);
        }
        if let Some(p) = &f.payment_method {
            qb.push(" AND COALESCE(fe.method, 'Cash') = ").push_bind(p);
        }
        if let Some(q) = &f.q {
            qb.push(" AND (fe.driver_name ILIKE '%' || ")
                .push_bind(q)
                .push(" || '%' OR fe.car_no_plate ILIKE '%' || ")
                .push_bind(q)
                .push(" || '%' OR fe.transporter ILIKE '%' || ")
                .push_bind(q)
                .push(" || '%')");
        }
    }

    if f.wants_loans() {
        // Loans registered by banksms itself are excluded: the bank
        // transaction row already represents that money. Including both would
        // double-count every advance recorded from an SMS.
        qb.push(
            " UNION ALL SELECT (l.id + 2000000000000)::bigint, 'loan', NULL::bigint, FALSE, \
             'out', COALESCE(l.amount, 0)::numeric, 'EGP', \
             (l.date::date::timestamp AT TIME ZONE 'Africa/Cairo'), NULL, \
             COALESCE(d.name, e.name), NULL, 'Loan', l.description, \
             COALESCE(l.method, 'Cash'), NULL, NULL::bigint, NULL, \
             l.driver_id::bigint, l.employee_id::bigint, NULL::bigint, \
             COALESCE(l.method, ''), 1, NULL, NULL, NULL::timestamptz, \
             l.created_at AT TIME ZONE 'UTC', l.updated_at AT TIME ZONE 'UTC' \
             FROM public.loans l \
             LEFT JOIN public.drivers d ON d.id = l.driver_id \
             LEFT JOIN public.employees e ON e.id = l.employee_id \
             WHERE l.deleted_at IS NULL AND l.date IS NOT NULL \
             AND COALESCE(l.method, '') <> 'banksms'",
        );
        if let Some(from) = &f.from {
            qb.push(" AND (l.date::date::timestamp AT TIME ZONE 'Africa/Cairo') >= ")
                .push_bind(from);
        }
        if let Some(to) = &f.to {
            qb.push(" AND (l.date::date::timestamp AT TIME ZONE 'Africa/Cairo') <= ")
                .push_bind(to);
        }
        if let Some(p) = &f.payment_method {
            qb.push(" AND COALESCE(l.method, 'Cash') = ").push_bind(p);
        }
        if let Some(q) = &f.q {
            qb.push(" AND (l.description ILIKE '%' || ")
                .push_bind(q)
                .push(" || '%' OR COALESCE(d.name, e.name) ILIKE '%' || ")
                .push_bind(q)
                .push(" || '%')");
        }
    }

    qb.push(") u");
}

fn row_to_view(r: &sqlx::postgres::PgRow, loans: &HashMap<i64, LoanInfo>) -> TransactionView {
    let source: String = r.get("source");
    let amount: Decimal = r.get("amount");
    let is_bank = matches!(source.as_str(), "whatsapp" | "import" | "manual");
    let (principal, fee) = if is_bank {
        let (p, f) = derive_fee(amount);
        if f > Decimal::ZERO {
            (Some(p), Some(f))
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };
    let loan_id: Option<i64> = r.get("loan_id");
    TransactionView {
        id: r.get("id"),
        source,
        raw_message_id: r.get("raw_message_id"),
        direction: r.get("direction"),
        amount,
        currency: r.get("currency"),
        occurred_at: r.get("occurred_at"),
        account: r.get("account"),
        counterparty: r.get("counterparty"),
        reference: r.get("reference"),
        category: r.get("category"),
        description: r.get("description"),
        payment_method: r.get("payment_method"),
        company: r.get("company"),
        car_id: r.get("car_id"),
        car_no_plate: r.get("car_no_plate"),
        driver_id: r.get("driver_id"),
        employee_id: r.get("employee_id"),
        paid_by: r.get("paid_by"),
        loan: loan_id.and_then(|id| loans.get(&id).cloned()),
        principal,
        fee,
        version: r.get("version"),
        editable: r.get("editable"),
        edited_by: r.get("edited_by"),
        edited_at: r.get("edited_at"),
        created_by: r.get("created_by"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

/// Fetch LoanInfo for every loan_id in the page, one query.
async fn loans_for(
    pool: &PgPool,
    rows: &[sqlx::postgres::PgRow],
) -> AppResult<HashMap<i64, LoanInfo>> {
    let ids: Vec<i64> = rows
        .iter()
        .filter_map(|r| r.get::<Option<i64>, _>("loan_id"))
        .collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let loans = sqlx::query(
        "SELECT id::bigint AS id, kind, is_paid FROM public.loans
         WHERE id = ANY($1) AND deleted_at IS NULL",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?;
    Ok(loans
        .into_iter()
        .map(|r| {
            let id: i64 = r.get("id");
            (
                id,
                LoanInfo {
                    id,
                    kind: r.get("kind"),
                    is_paid: r.get("is_paid"),
                },
            )
        })
        .collect())
}

fn parse_cursor(cursor: &Option<String>) -> AppResult<Option<(DateTime<Utc>, i64)>> {
    let Some(c) = cursor else { return Ok(None) };
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
    Ok(Some((ts, id)))
}

/* ------------------------------------------------------------------------ */
/* Handlers                                                                  */
/* ------------------------------------------------------------------------ */

pub async fn list(
    pool: web::Data<PgPool>,
    query: web::Query<ListQuery>,
) -> AppResult<HttpResponse> {
    let f = query.into_inner();
    let limit = f.limit.unwrap_or(100).min(200) as i64;
    let cursor = parse_cursor(&f.cursor)?;

    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("SELECT u.* FROM ");
    push_union(&mut qb, &f);
    qb.push(" WHERE TRUE");
    if let Some((ts, id)) = cursor {
        qb.push(" AND (u.occurred_at, u.id) < (")
            .push_bind(ts)
            .push(", ")
            .push_bind(id)
            .push(")");
    }
    qb.push(" ORDER BY u.occurred_at DESC, u.id DESC LIMIT ");
    qb.push_bind(limit + 1);

    let rows = qb.build().fetch_all(pool.get_ref()).await?;
    let has_more = rows.len() as i64 > limit;
    let rows = &rows[..rows.len().min(limit as usize)];

    let loans = loans_for(pool.get_ref(), rows).await?;
    let data: Vec<TransactionView> = rows.iter().map(|r| row_to_view(r, &loans)).collect();
    let next_cursor = if has_more {
        data.last()
            .map(|v| format!("{}:{}", v.occurred_at.timestamp_millis(), v.id))
    } else {
        None
    };

    Ok(HttpResponse::Ok().json(Page { data, next_cursor }))
}

/* ---------------------------- statistics --------------------------------- */

#[derive(Debug, Serialize)]
struct DateBucket {
    date: String,
    out: Decimal,
    #[serde(rename = "in")]
    inflow: Decimal,
    count: i64,
}

#[derive(Debug, Serialize)]
struct CategoryBucket {
    key: Option<String>,
    label: Option<String>,
    label_ar: Option<String>,
    out: Decimal,
    count: i64,
}

#[derive(Debug, Serialize)]
struct PartyBucket {
    driver_id: Option<i64>,
    employee_id: Option<i64>,
    name: Option<String>,
    kind: String,
    total: Decimal,
    count: i64,
}

#[derive(Debug, Serialize)]
struct Statistics {
    count: i64,
    total_in: Decimal,
    total_out: Decimal,
    net: Decimal,
    total_fees: Decimal,
    by_date: Vec<DateBucket>,
    by_category: Vec<CategoryBucket>,
    by_party: Vec<PartyBucket>,
}

pub async fn statistics(
    pool: web::Data<PgPool>,
    query: web::Query<ListQuery>,
) -> AppResult<HttpResponse> {
    let f = query.into_inner();

    // Totals + per-day + per-category in one pass over the union.
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT (u.occurred_at AT TIME ZONE 'Africa/Cairo')::date::text AS cairo_day, \
         u.category, u.direction, u.amount, u.source FROM ",
    );
    push_union(&mut qb, &f);
    let rows = qb.build().fetch_all(pool.get_ref()).await?;

    let mut total_in = Decimal::ZERO;
    let mut total_out = Decimal::ZERO;
    let mut total_fees = Decimal::ZERO;
    let mut by_date: HashMap<String, DateBucket> = HashMap::new();
    let mut by_cat: HashMap<Option<String>, CategoryBucket> = HashMap::new();

    for r in &rows {
        let day: String = r.get("cairo_day");
        let category: Option<String> = r.get("category");
        let direction: String = r.get("direction");
        let amount: Decimal = r.get("amount");
        let source: String = r.get("source");

        let d = by_date.entry(day.clone()).or_insert_with(|| DateBucket {
            date: day,
            out: Decimal::ZERO,
            inflow: Decimal::ZERO,
            count: 0,
        });
        d.count += 1;
        if direction == "in" {
            d.inflow += amount;
            total_in += amount;
        } else {
            d.out += amount;
            total_out += amount;
            let c = by_cat
                .entry(category.clone())
                .or_insert_with(|| CategoryBucket {
                    key: category.clone(),
                    label: None,
                    label_ar: None,
                    out: Decimal::ZERO,
                    count: 0,
                });
            c.out += amount;
            c.count += 1;
        }
        if matches!(source.as_str(), "whatsapp" | "import" | "manual") {
            let (_, fee) = derive_fee(amount);
            total_fees += fee;
        }
    }

    // Labels for the category buckets.
    let cats = sqlx::query("SELECT key, label, label_ar FROM banksms.categories")
        .fetch_all(pool.get_ref())
        .await?;
    for c in &cats {
        let key: String = c.get("key");
        if let Some(bucket) = by_cat.get_mut(&Some(key)) {
            bucket.label = Some(c.get("label"));
            bucket.label_ar = Some(c.get("label_ar"));
        }
    }

    // Who owes what: bank rows in posting categories, grouped by person.
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT t.driver_id, t.employee_id, COALESCE(d.name, e.name, t.paid_by) AS name, \
         c.posting_kind AS kind, SUM(t.amount)::numeric AS total, COUNT(*) AS count \
         FROM banksms.transactions t \
         JOIN banksms.categories c ON lower(c.key) = lower(t.category) AND c.posting_kind IS NOT NULL \
         LEFT JOIN public.drivers d ON d.id = t.driver_id \
         LEFT JOIN public.employees e ON e.id = t.employee_id \
         WHERE t.deleted_at IS NULL",
    );
    if let Some(from) = &f.from {
        qb.push(" AND t.occurred_at >= ").push_bind(from);
    }
    if let Some(to) = &f.to {
        qb.push(" AND t.occurred_at <= ").push_bind(to);
    }
    qb.push(" GROUP BY t.driver_id, t.employee_id, COALESCE(d.name, e.name, t.paid_by), c.posting_kind ORDER BY total DESC");
    let party_rows = qb.build().fetch_all(pool.get_ref()).await?;

    let by_party = party_rows
        .iter()
        .map(|r| PartyBucket {
            driver_id: r.get("driver_id"),
            employee_id: r.get("employee_id"),
            name: r.get("name"),
            kind: r.get("kind"),
            total: r.get("total"),
            count: r.get("count"),
        })
        .collect();

    let mut by_date: Vec<DateBucket> = by_date.into_values().collect();
    by_date.sort_by(|a, b| a.date.cmp(&b.date));
    let mut by_category: Vec<CategoryBucket> = by_cat.into_values().collect();
    by_category.sort_by(|a, b| b.out.cmp(&a.out));

    Ok(HttpResponse::Ok().json(Statistics {
        count: rows.len() as i64,
        total_in,
        total_out,
        net: total_in - total_out,
        total_fees,
        by_date,
        by_category,
        by_party,
    }))
}

/* ------------------------------ export ----------------------------------- */

pub async fn export(
    pool: web::Data<PgPool>,
    query: web::Query<ListQuery>,
) -> AppResult<HttpResponse> {
    use rust_xlsxwriter::{Format, Workbook};

    let f = query.into_inner();
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("SELECT u.* FROM ");
    push_union(&mut qb, &f);
    qb.push(" ORDER BY u.occurred_at DESC, u.id DESC LIMIT 10000");
    let rows = qb.build().fetch_all(pool.get_ref()).await?;
    let loans = loans_for(pool.get_ref(), &rows).await?;

    let mut wb = Workbook::new();
    let sheet = wb.add_worksheet();
    sheet
        .set_name("Expenses")
        .map_err(|e| AppError::Internal(e.to_string()))?;
    if flag(&f.rtl, false) {
        sheet.set_right_to_left(true);
    }

    let bold = Format::new().set_bold();
    let headers = [
        "Date (Cairo)",
        "Direction",
        "Amount",
        "Currency",
        "Category",
        "Counterparty",
        "Description",
        "Account",
        "Reference",
        "Company",
        "Vehicle",
        "Paid by / Party",
        "Payment method",
        "Source",
    ];
    for (col, h) in headers.iter().enumerate() {
        sheet
            .write_with_format(0, col as u16, *h, &bold)
            .map_err(|e| AppError::Internal(e.to_string()))?;
    }

    for (i, r) in rows.iter().enumerate() {
        let v = row_to_view(r, &loans);
        let row = (i + 1) as u32;
        let cairo = v
            .occurred_at
            .with_timezone(&chrono_tz::Africa::Cairo)
            .format("%Y-%m-%d %H:%M")
            .to_string();
        let w = |sheet: &mut rust_xlsxwriter::Worksheet, col: u16, s: String| {
            sheet
                .write(row, col, s)
                .map(|_| ())
                .map_err(|e| AppError::Internal(e.to_string()))
        };
        w(sheet, 0, cairo)?;
        w(sheet, 1, v.direction.clone())?;
        // Amount as a real number cell so Excel can sum it. f64 for DISPLAY
        // only — the stored value remains NUMERIC/Decimal end to end.
        sheet
            .write(row, 2, f64::from_str(&v.amount.to_string()).unwrap_or(0.0))
            .map_err(|e| AppError::Internal(e.to_string()))?;
        w(sheet, 3, v.currency.clone())?;
        w(sheet, 4, v.category.clone().unwrap_or_default())?;
        w(sheet, 5, v.counterparty.clone().unwrap_or_default())?;
        w(sheet, 6, v.description.clone().unwrap_or_default())?;
        w(sheet, 7, v.account.clone().unwrap_or_default())?;
        w(sheet, 8, v.reference.clone().unwrap_or_default())?;
        w(sheet, 9, v.company.clone().unwrap_or_default())?;
        w(sheet, 10, v.car_no_plate.clone().unwrap_or_default())?;
        w(sheet, 11, v.paid_by.clone().unwrap_or_default())?;
        w(sheet, 12, v.payment_method.clone().unwrap_or_default())?;
        w(sheet, 13, v.source.clone())?;
    }
    sheet.autofit();

    let bytes = wb
        .save_to_buffer()
        .map_err(|e| AppError::Internal(e.to_string()))?;

    let fname = format!(
        "expenses_{}_{}.xlsx",
        f.from
            .map(|d| d.format("%Y%m%d").to_string())
            .unwrap_or_else(|| "start".into()),
        f.to.map(|d| d.format("%Y%m%d").to_string())
            .unwrap_or_else(|| "now".into()),
    );
    Ok(HttpResponse::Ok()
        .content_type("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet")
        .insert_header((
            "Content-Disposition",
            format!("attachment; filename=\"{fname}\""),
        ))
        .body(bytes))
}

/* ------------------------------- CRUD ------------------------------------ */

#[derive(Debug, Deserialize)]
pub struct CreateBody {
    pub raw_message_id: Option<i64>,
    pub direction: String,
    pub amount: String,
    pub currency: Option<String>,
    pub occurred_at: DateTime<Utc>,
    pub account: Option<String>,
    pub counterparty: Option<String>,
    pub reference: Option<String>,
    pub category: Option<String>,
    pub description: Option<String>,
    pub payment_method: Option<String>,
    pub company: Option<String>,
    pub car_id: Option<i64>,
    pub driver_id: Option<i64>,
    pub employee_id: Option<i64>,
    pub paid_by: Option<String>,
}

fn parse_amount(s: &str) -> AppResult<Decimal> {
    let d = Decimal::from_str(s.trim())
        .map_err(|_| AppError::BadRequest(format!("'{s}' is not a valid amount")))?;
    if d <= Decimal::ZERO {
        return Err(AppError::BadRequest("amount must be positive".into()));
    }
    Ok(d)
}

fn validate_direction(d: &str) -> AppResult<()> {
    if d == "in" || d == "out" {
        Ok(())
    } else {
        Err(AppError::BadRequest(
            "direction must be 'in' or 'out'".into(),
        ))
    }
}

pub async fn create(
    pool: web::Data<PgPool>,
    req: HttpRequest,
    body: web::Json<CreateBody>,
) -> AppResult<HttpResponse> {
    let ctx = super::ctx(&req)?;
    let b = body.into_inner();
    validate_direction(&b.direction)?;
    let amount = parse_amount(&b.amount)?;
    if b.driver_id.is_some() && b.employee_id.is_some() {
        return Err(AppError::BadRequest(
            "a transaction belongs to a driver or an employee, not both".into(),
        ));
    }

    let mut tx = pool.begin().await?;

    if let Some(raw_id) = b.raw_message_id {
        // Promotion of an ignored message: the link is what makes the human
        // decision terminal and visible. One transaction per message, ever.
        let exists: Option<i64> =
            sqlx::query_scalar("SELECT id FROM banksms.raw_messages WHERE id = $1")
                .bind(raw_id)
                .fetch_optional(&mut *tx)
                .await?;
        if exists.is_none() {
            return Err(AppError::NotFound(format!("message {raw_id}")));
        }
        let taken: Option<i64> =
            sqlx::query_scalar("SELECT id FROM banksms.transactions WHERE raw_message_id = $1")
                .bind(raw_id)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(t) = taken {
            return Err(AppError::Conflict(format!(
                "message {raw_id} is already recorded as transaction {t}"
            )));
        }
    }

    let row = sqlx::query(
        r#"
        INSERT INTO banksms.transactions
            (source, raw_message_id, direction, amount, currency, occurred_at,
             account, counterparty, reference, category, description,
             payment_method, company, car_id, driver_id, employee_id, paid_by,
             created_by)
        VALUES ('manual', $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                $13, $14, $15, $16, $17)
        RETURNING id
        "#,
    )
    .bind(b.raw_message_id)
    .bind(&b.direction)
    .bind(amount)
    .bind(b.currency.as_deref().unwrap_or("EGP"))
    .bind(b.occurred_at)
    .bind(&b.account)
    .bind(&b.counterparty)
    .bind(&b.reference)
    .bind(&b.category)
    .bind(&b.description)
    .bind(&b.payment_method)
    .bind(&b.company)
    .bind(b.car_id)
    .bind(b.driver_id)
    .bind(b.employee_id)
    .bind(&b.paid_by)
    .bind(ctx.actor())
    .fetch_one(&mut *tx)
    .await?;
    let id: i64 = row.get("id");

    let reg = Registrable {
        id,
        amount,
        occurred_at: b.occurred_at,
        description: b.description.clone(),
        category: b.category.clone(),
        driver_id: b.driver_id,
        employee_id: b.employee_id,
        loan_id: None,
    };
    let rule = registration::load_rule(&mut tx, reg.category.as_deref()).await?;
    registration::validate(rule.as_ref(), &reg)?;
    registration::reconcile(&mut tx, &reg).await?;

    tx.commit().await?;

    let view = fetch_view(pool.get_ref(), id).await?;
    Ok(HttpResponse::Created().json(view))
}

/// Load one bank transaction as a view (not fuel/loan synthetics).
async fn fetch_view(pool: &PgPool, id: i64) -> AppResult<TransactionView> {
    let row = sqlx::query(
        "SELECT t.id, t.source, t.raw_message_id, TRUE AS editable, t.direction, t.amount, \
         t.currency, t.occurred_at, t.account, t.counterparty, t.reference, t.category, \
         t.description, t.payment_method, t.company, t.car_id, t.car_no_plate, t.driver_id, \
         t.employee_id, t.loan_id, t.paid_by, t.version, t.created_by, t.edited_by, \
         t.edited_at, t.created_at, t.updated_at \
         FROM banksms.transactions t WHERE t.id = $1 AND t.deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound(format!("transaction {id}")))?;

    let loans = loans_for(pool, std::slice::from_ref(&row)).await?;
    Ok(row_to_view(&row, &loans))
}

#[derive(Debug, Serialize)]
struct DetailView {
    #[serde(flatten)]
    view: TransactionView,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_wa_timestamp: Option<DateTime<Utc>>,
}

pub async fn get(pool: web::Data<PgPool>, path: web::Path<i64>) -> AppResult<HttpResponse> {
    let id = path.into_inner();
    let view = fetch_view(pool.get_ref(), id).await?;
    let (raw_body, raw_wa_timestamp) = match view.raw_message_id {
        Some(rid) => {
            let r =
                sqlx::query("SELECT body, wa_timestamp FROM banksms.raw_messages WHERE id = $1")
                    .bind(rid)
                    .fetch_optional(pool.get_ref())
                    .await?;
            match r {
                Some(r) => (Some(r.get("body")), Some(r.get("wa_timestamp"))),
                None => (None, None),
            }
        }
        None => (None, None),
    };
    Ok(HttpResponse::Ok().json(DetailView {
        view,
        raw_body,
        raw_wa_timestamp,
    }))
}

/// If-Match: mandatory on PATCH/DELETE. Missing → 428, stale → 409.
fn if_match(req: &HttpRequest) -> AppResult<i32> {
    let header = req
        .headers()
        .get("If-Match")
        .ok_or(AppError::PreconditionRequired)?
        .to_str()
        .map_err(|_| AppError::BadRequest("unreadable If-Match header".into()))?
        .trim()
        .trim_matches('"');
    header
        .parse()
        .map_err(|_| AppError::BadRequest("If-Match must be the row version".into()))
}

/// Double-Option per nullable field: absent = leave alone, null = clear.
#[derive(Debug, Default, Deserialize)]
pub struct PatchBody {
    pub direction: Option<String>,
    pub amount: Option<String>,
    pub currency: Option<String>,
    pub occurred_at: Option<DateTime<Utc>>,
    #[serde(default, with = "::serde_with::rust::double_option")]
    pub account: Option<Option<String>>,
    #[serde(default, with = "::serde_with::rust::double_option")]
    pub counterparty: Option<Option<String>>,
    #[serde(default, with = "::serde_with::rust::double_option")]
    pub reference: Option<Option<String>>,
    #[serde(default, with = "::serde_with::rust::double_option")]
    pub category: Option<Option<String>>,
    #[serde(default, with = "::serde_with::rust::double_option")]
    pub description: Option<Option<String>>,
    #[serde(default, with = "::serde_with::rust::double_option")]
    pub payment_method: Option<Option<String>>,
    #[serde(default, with = "::serde_with::rust::double_option")]
    pub company: Option<Option<String>>,
    #[serde(default, with = "::serde_with::rust::double_option")]
    pub car_id: Option<Option<i64>>,
    #[serde(default, with = "::serde_with::rust::double_option")]
    pub driver_id: Option<Option<i64>>,
    #[serde(default, with = "::serde_with::rust::double_option")]
    pub employee_id: Option<Option<i64>>,
    #[serde(default, with = "::serde_with::rust::double_option")]
    pub paid_by: Option<Option<String>>,
}

pub async fn patch(
    pool: web::Data<PgPool>,
    req: HttpRequest,
    path: web::Path<i64>,
    body: web::Json<PatchBody>,
) -> AppResult<HttpResponse> {
    let ctx = super::ctx(&req)?;
    let id = path.into_inner();
    let expected = if_match(&req)?;
    let b = body.into_inner();

    let mut tx = pool.begin().await?;

    let row = sqlx::query(
        "SELECT direction, amount, currency, occurred_at, account, counterparty, reference,
                category, description, payment_method, company, car_id, driver_id,
                employee_id, paid_by, loan_id, version
         FROM banksms.transactions
         WHERE id = $1 AND deleted_at IS NULL AND source <> '' FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound(format!("transaction {id}")))?;

    let actual: i32 = row.get("version");
    if actual != expected {
        return Err(AppError::VersionConflict { expected, actual });
    }

    // Merge: patch wins where present.
    let direction: String = b.direction.unwrap_or_else(|| row.get("direction"));
    validate_direction(&direction)?;
    let amount: Decimal = match &b.amount {
        Some(s) => parse_amount(s)?,
        None => row.get("amount"),
    };
    let currency: String = b.currency.unwrap_or_else(|| row.get("currency"));
    let occurred_at: DateTime<Utc> = b.occurred_at.unwrap_or_else(|| row.get("occurred_at"));
    let account: Option<String> = b.account.unwrap_or_else(|| row.get("account"));
    let counterparty: Option<String> = b.counterparty.unwrap_or_else(|| row.get("counterparty"));
    let reference: Option<String> = b.reference.unwrap_or_else(|| row.get("reference"));
    let category: Option<String> = b.category.unwrap_or_else(|| row.get("category"));
    let description: Option<String> = b.description.unwrap_or_else(|| row.get("description"));
    let payment_method: Option<String> = b
        .payment_method
        .unwrap_or_else(|| row.get("payment_method"));
    let company: Option<String> = b.company.unwrap_or_else(|| row.get("company"));
    let car_id: Option<i64> = b.car_id.unwrap_or_else(|| row.get("car_id"));
    let driver_id: Option<i64> = b.driver_id.unwrap_or_else(|| row.get("driver_id"));
    let employee_id: Option<i64> = b.employee_id.unwrap_or_else(|| row.get("employee_id"));
    let paid_by: Option<String> = b.paid_by.unwrap_or_else(|| row.get("paid_by"));

    if driver_id.is_some() && employee_id.is_some() {
        return Err(AppError::BadRequest(
            "a transaction belongs to a driver or an employee, not both".into(),
        ));
    }

    sqlx::query(
        r#"
        UPDATE banksms.transactions SET
            direction = $1, amount = $2, currency = $3, occurred_at = $4,
            account = $5, counterparty = $6, reference = $7, category = $8,
            description = $9, payment_method = $10, company = $11, car_id = $12,
            driver_id = $13, employee_id = $14, paid_by = $15,
            version = version + 1, edited_by = $16, edited_at = now(), updated_at = now()
        WHERE id = $17
        "#,
    )
    .bind(&direction)
    .bind(amount)
    .bind(&currency)
    .bind(occurred_at)
    .bind(&account)
    .bind(&counterparty)
    .bind(&reference)
    .bind(&category)
    .bind(&description)
    .bind(&payment_method)
    .bind(&company)
    .bind(car_id)
    .bind(driver_id)
    .bind(employee_id)
    .bind(&paid_by)
    .bind(ctx.actor())
    .bind(id)
    .execute(&mut *tx)
    .await?;

    let reg = Registrable {
        id,
        amount,
        occurred_at,
        description: description.clone(),
        category: category.clone(),
        driver_id,
        employee_id,
        loan_id: row.get("loan_id"),
    };
    let rule = registration::load_rule(&mut tx, reg.category.as_deref()).await?;
    registration::validate(rule.as_ref(), &reg)?;
    registration::reconcile(&mut tx, &reg).await?;

    tx.commit().await?;

    let view = fetch_view(pool.get_ref(), id).await?;
    Ok(HttpResponse::Ok().json(view))
}

pub async fn delete(
    pool: web::Data<PgPool>,
    req: HttpRequest,
    path: web::Path<i64>,
) -> AppResult<HttpResponse> {
    let _ctx = super::ctx(&req)?;
    let id = path.into_inner();
    let expected = if_match(&req)?;

    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT version, amount, occurred_at, description, category, driver_id,
                employee_id, loan_id
         FROM banksms.transactions WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound(format!("transaction {id}")))?;

    let actual: i32 = row.get("version");
    if actual != expected {
        return Err(AppError::VersionConflict { expected, actual });
    }

    let reg = Registrable {
        id,
        amount: row.get("amount"),
        occurred_at: row.get("occurred_at"),
        description: row.get("description"),
        category: row.get("category"),
        driver_id: row.get("driver_id"),
        employee_id: row.get("employee_id"),
        loan_id: row.get("loan_id"),
    };
    registration::unregister_for_delete(&mut tx, &reg).await?;

    sqlx::query(
        "UPDATE banksms.transactions
         SET deleted_at = now(), version = version + 1, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(HttpResponse::NoContent().finish())
}
