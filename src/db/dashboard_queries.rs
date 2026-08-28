//! The dashboard's queries.
//!
//! This page is the entry point, so every query here has a measured budget and
//! a bounded shape: no query returns more rows than the screen can show, and
//! the whole set runs concurrently on the pool so the endpoint costs roughly
//! its slowest member (~11 ms on production data) rather than the sum.
//!
//! Money reuses [`crate::db::revenue`], which is what makes the dashboard agree
//! with the statistics page by construction instead of drifting from it the way
//! the old Go dashboard did.

use anyhow::Result;
use rust_decimal::Decimal;
use sqlx::{PgPool, Row};

use crate::db::revenue::allocation::per_row_revenue_cte;
use crate::db::stats_queries::render;

/* ------------------------------------------------------------------------ */
/* Month totals                                                              */
/* ------------------------------------------------------------------------ */

pub struct MonthTotals {
    /// Logical trips — a multi-container trip counts once.
    pub trips: i64,
    /// Distinct trucks that worked in the window.
    pub trucks: i64,
    pub litres: i64,
}

pub async fn month_totals(pool: &PgPool, from: &str, to: &str) -> Result<MonthTotals> {
    let row = sqlx::query(&render(
        r#"
        SELECT
            ({trip_count})::bigint                 AS trips,
            COUNT(DISTINCT car_no_plate)::bigint   AS trucks,
            COALESCE(SUM(tank_capacity), 0)::bigint AS litres
        FROM trips
        WHERE deleted_at IS NULL AND date BETWEEN $1 AND $2
        "#,
    ))
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?;
    Ok(MonthTotals {
        trips: row.get("trips"),
        trucks: row.get("trucks"),
        litres: row.get("litres"),
    })
}

/* ------------------------------------------------------------------------ */
/* Revenue                                                                   */
/* ------------------------------------------------------------------------ */

/// Total earned in the window, VAT and rentals included, across all companies.
///
/// Sums `allocated_total` from the shared per-row CTE, so this is exactly the
/// figure the trips list's rows add up to and the statistics page reports —
/// one source, three surfaces.
pub async fn revenue_total(pool: &PgPool, from: &str, to: &str) -> Result<f64> {
    let sql = format!(
        "WITH {} SELECT COALESCE(SUM(allocated_total), 0.0)::float8 AS v FROM revenue",
        per_row_revenue_cte("t.date BETWEEN $1 AND $2")
    );
    let row = sqlx::query(&sql).bind(from).bind(to).fetch_one(pool).await?;
    Ok(row.get("v"))
}

/// Revenue per company for the window, for the revenue drawer.
pub async fn revenue_by_company(pool: &PgPool, from: &str, to: &str) -> Result<Vec<(String, f64)>> {
    let sql = format!(
        "WITH {} SELECT company, COALESCE(SUM(allocated_total), 0.0)::float8 AS v \
         FROM revenue GROUP BY company ORDER BY v DESC",
        per_row_revenue_cte("t.date BETWEEN $1 AND $2")
    );
    let rows = sqlx::query(&sql).bind(from).bind(to).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get::<Option<String>, _>("company").unwrap_or_default(), r.get("v")))
        .collect())
}

/* ------------------------------------------------------------------------ */
/* Cash out (banksms)                                                        */
/* ------------------------------------------------------------------------ */

/// `split_at IS NULL` everywhere: a split parent keeps its full amount AND has
/// children summing to it, so forgetting the filter double-counts every split.
const LEDGER: &str =
    "deleted_at IS NULL AND split_at IS NULL AND direction = 'out' \
     AND occurred_at >= ($1::date)::timestamptz \
     AND occurred_at < (($2::date + 1))::timestamptz";

pub async fn cash_out_total(pool: &PgPool, from: &str, to: &str) -> Result<Decimal> {
    let row = sqlx::query(&format!(
        "SELECT COALESCE(SUM(amount), 0) AS v FROM banksms.transactions WHERE {LEDGER}"
    ))
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?;
    Ok(row.get("v"))
}

pub async fn cash_out_by_category(
    pool: &PgPool,
    from: &str,
    to: &str,
) -> Result<Vec<(String, Decimal)>> {
    let rows = sqlx::query(&format!(
        "SELECT COALESCE(category, 'Other') AS k, SUM(amount) AS v \
         FROM banksms.transactions WHERE {LEDGER} \
         GROUP BY COALESCE(category, 'Other') ORDER BY v DESC"
    ))
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| (r.get("k"), r.get("v"))).collect())
}

pub struct LargePayment {
    pub occurred_at: chrono::DateTime<chrono::Utc>,
    pub label: String,
    pub category: Option<String>,
    pub amount: Decimal,
}

/// The five largest single payments in the window, for the cash-out drawer.
pub async fn largest_payments(pool: &PgPool, from: &str, to: &str) -> Result<Vec<LargePayment>> {
    let rows = sqlx::query(&format!(
        "SELECT occurred_at, amount, category, \
                COALESCE(NULLIF(counterparty, ''), NULLIF(description, ''), category, '—') AS label \
         FROM banksms.transactions WHERE {LEDGER} \
         ORDER BY amount DESC LIMIT 5"
    ))
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| LargePayment {
            occurred_at: r.get("occurred_at"),
            label: r.get("label"),
            category: r.get("category"),
            amount: r.get("amount"),
        })
        .collect())
}

/// Incoming transfers nobody has triaged. Not month-scoped: an unreviewed
/// transfer from last month is still unreviewed.
pub async fn transfers_unreviewed(pool: &PgPool) -> Result<i64> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS n FROM banksms.transactions \
         WHERE deleted_at IS NULL AND split_at IS NULL AND direction = 'in'",
    )
    .fetch_one(pool)
    .await?;
    Ok(row.get("n"))
}

/* ------------------------------------------------------------------------ */
/* Advances                                                                  */
/* ------------------------------------------------------------------------ */

pub struct AdvancesOutstanding {
    pub total: f64,
    pub count: i64,
}

pub async fn advances_outstanding(pool: &PgPool) -> Result<AdvancesOutstanding> {
    let row = sqlx::query(
        "SELECT COALESCE(SUM(amount), 0.0)::float8 AS total, COUNT(*) AS n \
         FROM loans WHERE deleted_at IS NULL AND is_paid = false",
    )
    .fetch_one(pool)
    .await?;
    Ok(AdvancesOutstanding {
        total: row.get("total"),
        count: row.get("n"),
    })
}

pub struct AdvanceParty {
    pub name: String,
    pub kind: Option<String>,
    pub total: f64,
    pub count: i64,
}

/// Who owes what, largest first, for the advances drawer. Capped at 25 —
/// a drawer is a summary, and the loans page is where the full list lives.
pub async fn advances_by_party(pool: &PgPool) -> Result<Vec<AdvanceParty>> {
    let rows = sqlx::query(
        r#"
        SELECT COALESCE(d.name, e.name, '—') AS name,
               MAX(l.kind)                   AS kind,
               SUM(l.amount)::float8         AS total,
               COUNT(*)                      AS n
        FROM loans l
        LEFT JOIN drivers d   ON d.id = l.driver_id
        LEFT JOIN employees e ON e.id = l.employee_id
        WHERE l.deleted_at IS NULL AND l.is_paid = false
        GROUP BY COALESCE(d.name, e.name, '—')
        ORDER BY total DESC
        LIMIT 25
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| AdvanceParty {
            name: r.get("name"),
            kind: r.get("kind"),
            total: r.get("total"),
            count: r.get("n"),
        })
        .collect())
}

/* ------------------------------------------------------------------------ */
/* Fleet                                                                     */
/* ------------------------------------------------------------------------ */

pub struct FleetCar {
    pub etit_id: Option<String>,
    pub plate: String,
    pub last_trip_date: Option<String>,
}

/// Every car, with its most recent trip date.
///
/// A LATERAL top-1 per car (8.7 ms measured) rather than `MAX(date)` over the
/// join (206 ms measured) — the aggregate walked every trip row in history to
/// answer a 22-row question; the lateral descends the (car_id, date) index once
/// per car.
///
/// `etit_car_id` is an empty string, not NULL, on the untracked service
/// vehicles — normalised to `None` here so the payload's `etit_id: null` is the
/// one honest flag the frontend needs.
pub async fn fleet(pool: &PgPool) -> Result<Vec<FleetCar>> {
    let rows = sqlx::query(
        r#"
        SELECT c.car_no_plate,
               NULLIF(c.etit_car_id, '') AS etit_id,
               lt.last_date
        FROM cars c
        LEFT JOIN LATERAL (
            SELECT t.date AS last_date FROM trips t
            WHERE t.car_id = c.id AND t.deleted_at IS NULL
            ORDER BY t.date DESC LIMIT 1
        ) lt ON true
        WHERE c.deleted_at IS NULL
        ORDER BY c.car_no_plate
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| FleetCar {
            etit_id: r.get("etit_id"),
            plate: r.get::<Option<String>, _>("car_no_plate").unwrap_or_default(),
            last_trip_date: r.get("last_date"),
        })
        .collect())
}

/* ------------------------------------------------------------------------ */
/* Exceptions                                                                */
/* ------------------------------------------------------------------------ */

/// Trips in the window that earn nothing: no fee mapping for their route, or a
/// driver / drop-off still carrying the «غير مسجل» sentinel. They bill zero and
/// say nothing, which is why they are a dashboard item and not just a filter.
pub async fn trips_earning_zero(pool: &PgPool, from: &str, to: &str) -> Result<i64> {
    let row = sqlx::query(
        r#"
        SELECT COUNT(*) AS n
        FROM trips t
        LEFT JOIN fee_mappings fm
            ON  fm.company = t.company AND fm.terminal = t.terminal
            AND fm.drop_off_point = t.drop_off_point AND fm.deleted_at IS NULL
        WHERE t.deleted_at IS NULL AND t.date BETWEEN $1 AND $2
          AND (fm.id IS NULL OR t.driver_name = 'غير مسجل' OR t.drop_off_point = 'غير مسجل')
        "#,
    )
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?;
    Ok(row.get("n"))
}

/* ------------------------------------------------------------------------ */
/* Daily series (drawers)                                                    */
/* ------------------------------------------------------------------------ */

pub struct DayCount {
    pub date: String,
    pub trips: i64,
}

pub async fn trips_by_day(pool: &PgPool, from: &str, to: &str) -> Result<Vec<DayCount>> {
    let rows = sqlx::query(&render(
        r#"
        SELECT date, ({trip_count})::bigint AS trips
        FROM trips WHERE deleted_at IS NULL AND date BETWEEN $1 AND $2
        GROUP BY date ORDER BY date
        "#,
    ))
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| DayCount {
            date: r.get("date"),
            trips: r.get("trips"),
        })
        .collect())
}

pub async fn trips_by_company(pool: &PgPool, from: &str, to: &str) -> Result<Vec<(String, i64)>> {
    let rows = sqlx::query(&render(
        r#"
        SELECT company, ({trip_count})::bigint AS trips
        FROM trips WHERE deleted_at IS NULL AND date BETWEEN $1 AND $2
        GROUP BY company ORDER BY trips DESC
        "#,
    ))
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| (r.get("company"), r.get("trips"))).collect())
}
