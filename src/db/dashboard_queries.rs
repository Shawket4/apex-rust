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

/// The trips-side company scope. `$3 IS NULL` means "all companies", so one
/// prepared statement serves both shapes — cash-out and owed money have no
/// company dimension and never take this.
const COMPANY_SCOPE: &str = "($3::text IS NULL OR company = $3)";

pub async fn month_totals(
    pool: &PgPool,
    from: &str,
    to: &str,
    company: Option<&str>,
) -> Result<MonthTotals> {
    let row = sqlx::query(&render(&format!(
        r#"
        SELECT
            ({{trip_count}})::bigint                 AS trips,
            COUNT(DISTINCT car_no_plate)::bigint   AS trucks,
            COALESCE(SUM(tank_capacity), 0)::bigint AS litres
        FROM trips
        WHERE deleted_at IS NULL AND date BETWEEN $1 AND $2 AND {COMPANY_SCOPE}
        "#,
    )))
    .bind(from)
    .bind(to)
    .bind(company)
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
/// The same scope for queries built on the per-row revenue CTE, where trips
/// is aliased `t`.
const CTE_WINDOW: &str = "t.date BETWEEN $1 AND $2 AND ($3::text IS NULL OR t.company = $3)";

pub async fn revenue_total(
    pool: &PgPool,
    from: &str,
    to: &str,
    company: Option<&str>,
) -> Result<f64> {
    let sql = format!(
        "WITH {} SELECT COALESCE(SUM(allocated_total), 0.0)::float8 AS v FROM revenue",
        per_row_revenue_cte(CTE_WINDOW)
    );
    let row = sqlx::query(&sql).bind(from).bind(to).bind(company).fetch_one(pool).await?;
    Ok(row.get("v"))
}

/// Revenue per company for the window, for the revenue drawer.
pub async fn revenue_by_company(
    pool: &PgPool,
    from: &str,
    to: &str,
    company: Option<&str>,
) -> Result<Vec<(String, f64)>> {
    let sql = format!(
        "WITH {} SELECT company, COALESCE(SUM(allocated_total), 0.0)::float8 AS v \
         FROM revenue GROUP BY company ORDER BY v DESC",
        per_row_revenue_cte(CTE_WINDOW)
    );
    let rows = sqlx::query(&sql).bind(from).bind(to).bind(company).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get::<Option<String>, _>("company").unwrap_or_default(), r.get("v")))
        .collect())
}

/// Revenue per car for two specific days (today and yesterday on the fleet
/// tiles). Company-scoped like the headline revenue; keyed by plate, which is
/// how the fleet query identifies cars too.
pub async fn fleet_revenue(
    pool: &PgPool,
    from: &str,
    to: &str,
    company: Option<&str>,
) -> Result<Vec<(String, String, f64)>> {
    let sql = format!(
        "WITH {} SELECT car_no_plate, date, \
                COALESCE(SUM(allocated_total), 0.0)::float8 AS v \
         FROM revenue GROUP BY car_no_plate, date",
        per_row_revenue_cte(CTE_WINDOW)
    );
    let rows = sqlx::query(&sql).bind(from).bind(to).bind(company).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.get::<Option<String>, _>("car_no_plate").unwrap_or_default(),
                r.get::<Option<String>, _>("date").unwrap_or_default(),
                r.get("v"),
            )
        })
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

/// Transactions nobody has reviewed — money that moved and carries no
/// category, in either direction. That IS the review workflow here: a bank
/// message lands as an uncategorised transaction, and reviewing it means
/// saying what it was for. Counting only incoming transfers (the first cut of
/// this metric) said 5 while the ledger held ~280 unexplained payments.
///
/// Not month-scoped: an unreviewed transaction from last month is still
/// unreviewed.
pub async fn transactions_unreviewed(pool: &PgPool) -> Result<i64> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS n FROM banksms.transactions \
         WHERE deleted_at IS NULL AND split_at IS NULL AND category IS NULL",
    )
    .fetch_one(pool)
    .await?;
    Ok(row.get("n"))
}

/* ------------------------------------------------------------------------ */
/* Advances                                                                  */
/* ------------------------------------------------------------------------ */

/// Advances/loans issued in the window, split by who took them and what
/// they are.
///
/// Window-scoped, NOT `is_paid`-scoped: payroll recovers advances against
/// the month's salary outside this system (pay_slips is empty in production
/// and only 23 loan rows were ever flipped), so "outstanding" in any useful
/// sense means "issued in the period being looked at". `salary`-kind rows
/// are excluded everywhere: they are payroll visibility entries, not debt.
/// `method = 'banksms'` rows are excluded like the fleet-expenses union —
/// the bank transaction row already represents that money.
pub struct OwedBucket {
    pub is_driver: bool,
    pub kind: String,
    pub total: f64,
    pub count: i64,
}

pub async fn money_owed(pool: &PgPool, from: &str, to: &str) -> Result<Vec<OwedBucket>> {
    let rows = sqlx::query(
        "SELECT (driver_id IS NOT NULL) AS is_driver, kind, \
                COALESCE(SUM(amount), 0.0)::float8 AS total, COUNT(*) AS n \
         FROM loans \
         WHERE deleted_at IS NULL AND kind <> 'salary' AND date BETWEEN $1 AND $2 \
           AND COALESCE(method, '') <> 'banksms' \
         GROUP BY 1, 2",
    )
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| OwedBucket {
            is_driver: r.get("is_driver"),
            kind: r.get("kind"),
            total: r.get("total"),
            count: r.get("n"),
        })
        .collect())
}

/// Fuel spend in the window that the bank ledger cannot see. Same convention
/// as the fleet-expenses union (api/transactions.rs): PetroApp-synced rows
/// ONLY — a manually entered fuel event is paid through the bank, so its
/// money already arrives as a bank SMS; blending both would count the same
/// money twice.
pub async fn fuel_cash_out(pool: &PgPool, from: &str, to: &str) -> Result<f64> {
    let row = sqlx::query(
        "SELECT COALESCE(SUM(price), 0.0)::float8 AS v FROM fuel_events \
         WHERE deleted_at IS NULL AND date BETWEEN $1 AND $2 \
           AND petroapp_bill_id IS NOT NULL",
    )
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?;
    Ok(row.get("v"))
}

/// Advances and loans issued (paid out) in the window — money that left the
/// till, whatever its repayment status today. Rows registered by banksms
/// itself are excluded, mirroring the fleet-expenses union: the bank
/// transaction row already represents that money.
pub async fn advances_issued(pool: &PgPool, from: &str, to: &str) -> Result<f64> {
    let row = sqlx::query(
        "SELECT COALESCE(SUM(amount), 0.0)::float8 AS v FROM loans \
         WHERE deleted_at IS NULL AND kind <> 'salary' AND date BETWEEN $1 AND $2 \
           AND COALESCE(method, '') <> 'banksms'",
    )
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?;
    Ok(row.get("v"))
}

pub struct AdvanceParty {
    pub name: String,
    pub kind: Option<String>,
    pub is_driver: bool,
    pub total: f64,
    pub count: i64,
}

/// Who owes what, largest first, for the advances drawer. Capped at 25 —
/// a drawer is a summary, and the loans page is where the full list lives.
/// One row per (person, kind) so a driver holding both an advance and a loan
/// shows both lines rather than a muddled MAX(kind).
pub async fn advances_by_party(
    pool: &PgPool,
    from: &str,
    to: &str,
) -> Result<Vec<AdvanceParty>> {
    let rows = sqlx::query(
        r#"
        SELECT COALESCE(d.name, e.name, '—')  AS name,
               l.kind                         AS kind,
               (l.driver_id IS NOT NULL)      AS is_driver,
               SUM(l.amount)::float8          AS total,
               COUNT(*)                       AS n
        FROM loans l
        LEFT JOIN drivers d   ON d.id = l.driver_id
        LEFT JOIN employees e ON e.id = l.employee_id
        WHERE l.deleted_at IS NULL AND l.kind <> 'salary'
          AND l.date BETWEEN $1 AND $2
          AND COALESCE(l.method, '') <> 'banksms'
        GROUP BY COALESCE(d.name, e.name, '—'), l.kind, (l.driver_id IS NOT NULL)
        ORDER BY total DESC
        LIMIT 25
        "#,
    )
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| AdvanceParty {
            name: r.get("name"),
            kind: r.get("kind"),
            is_driver: r.get("is_driver"),
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
pub async fn trips_earning_zero(
    pool: &PgPool,
    from: &str,
    to: &str,
    company: Option<&str>,
) -> Result<i64> {
    let row = sqlx::query(
        r#"
        SELECT COUNT(*) AS n
        FROM trips t
        LEFT JOIN fee_mappings fm
            ON  fm.company = t.company AND fm.terminal = t.terminal
            AND fm.drop_off_point = t.drop_off_point AND fm.deleted_at IS NULL
        WHERE t.deleted_at IS NULL AND t.date BETWEEN $1 AND $2
          AND ($3::text IS NULL OR t.company = $3)
          AND (fm.id IS NULL OR t.driver_name = 'غير مسجل' OR t.drop_off_point = 'غير مسجل')
        "#,
    )
    .bind(from)
    .bind(to)
    .bind(company)
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

pub async fn trips_by_day(
    pool: &PgPool,
    from: &str,
    to: &str,
    company: Option<&str>,
) -> Result<Vec<DayCount>> {
    let rows = sqlx::query(&render(&format!(
        r#"
        SELECT date, ({{trip_count}})::bigint AS trips
        FROM trips WHERE deleted_at IS NULL AND date BETWEEN $1 AND $2 AND {COMPANY_SCOPE}
        GROUP BY date ORDER BY date
        "#,
    )))
    .bind(from)
    .bind(to)
    .bind(company)
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

pub async fn trips_by_company(
    pool: &PgPool,
    from: &str,
    to: &str,
    company: Option<&str>,
) -> Result<Vec<(String, i64)>> {
    let rows = sqlx::query(&render(&format!(
        r#"
        SELECT company, ({{trip_count}})::bigint AS trips
        FROM trips WHERE deleted_at IS NULL AND date BETWEEN $1 AND $2 AND {COMPANY_SCOPE}
        GROUP BY company ORDER BY trips DESC
        "#,
    )))
    .bind(from)
    .bind(to)
    .bind(company)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| (r.get("company"), r.get("trips"))).collect())
}

/* ------------------------------------------------------------------------ */
/* Fuel                                                                      */
/* ------------------------------------------------------------------------ */

/// The fuel CARD is a consumption view: every event counts, whatever paid
/// for it. (Unlike `cash_out`, which dedups manual fuel against the bank
/// ledger — different question, different number, both correct.)
pub struct FuelTotals {
    pub spend: f64,
    pub liters: f64,
    pub events: i64,
}

pub async fn fuel_totals(pool: &PgPool, from: &str, to: &str) -> Result<FuelTotals> {
    let row = sqlx::query(
        "SELECT COALESCE(SUM(price), 0.0)::float8 AS spend, \
                COALESCE(SUM(liters), 0.0)::float8 AS liters, COUNT(*) AS n \
         FROM fuel_events WHERE deleted_at IS NULL AND date BETWEEN $1 AND $2",
    )
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?;
    Ok(FuelTotals {
        spend: row.get("spend"),
        liters: row.get("liters"),
        events: row.get("n"),
    })
}

pub struct FuelEventRow {
    pub id: i64,
    pub car_no_plate: String,
    pub driver_name: String,
    pub date: String,
    pub time: String,
    pub liters: f64,
    pub price_per_liter: f64,
    pub price: f64,
    pub method: String,
    pub fuel_rate: f64,
}

/// Latest events first. `within` bounds the window for the drawer; the main
/// payload passes an open window and a small limit.
pub async fn recent_fuel_events(
    pool: &PgPool,
    from: &str,
    to: &str,
    limit: i64,
) -> Result<Vec<FuelEventRow>> {
    let rows = sqlx::query(
        "SELECT id, COALESCE(car_no_plate, '') AS car_no_plate, \
                COALESCE(driver_name, '') AS driver_name, \
                COALESCE(date, '') AS date, COALESCE(time, '') AS time, \
                COALESCE(liters, 0.0)::float8 AS liters, \
                COALESCE(price_per_liter, 0.0)::float8 AS price_per_liter, \
                COALESCE(price, 0.0)::float8 AS price, \
                COALESCE(NULLIF(method, ''), 'Cash') AS method, \
                COALESCE(fuel_rate, 0.0)::float8 AS fuel_rate \
         FROM fuel_events \
         WHERE deleted_at IS NULL AND date BETWEEN $1 AND $2 \
         ORDER BY date DESC, time DESC, id DESC LIMIT $3",
    )
    .bind(from)
    .bind(to)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| FuelEventRow {
            id: r.get::<i32, _>("id") as i64,
            car_no_plate: r.get("car_no_plate"),
            driver_name: r.get("driver_name"),
            date: r.get("date"),
            time: r.get("time"),
            liters: r.get("liters"),
            price_per_liter: r.get("price_per_liter"),
            price: r.get("price"),
            method: r.get("method"),
            fuel_rate: r.get("fuel_rate"),
        })
        .collect())
}

/// Spend and litres per payment method, for the fuel drawer.
pub async fn fuel_by_method(
    pool: &PgPool,
    from: &str,
    to: &str,
) -> Result<Vec<(String, f64, f64)>> {
    let rows = sqlx::query(
        "SELECT COALESCE(NULLIF(method, ''), 'Cash') AS m, \
                COALESCE(SUM(price), 0.0)::float8 AS spend, \
                COALESCE(SUM(liters), 0.0)::float8 AS liters \
         FROM fuel_events WHERE deleted_at IS NULL AND date BETWEEN $1 AND $2 \
         GROUP BY 1 ORDER BY spend DESC",
    )
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get("m"), r.get("spend"), r.get("liters")))
        .collect())
}

/// One page of fuel events in full detail — the dashboard's infinite list.
pub struct FullFuelEventRow {
    pub id: i64,
    pub car_id: Option<i64>,
    pub car_no_plate: String,
    pub driver_name: String,
    pub date: String,
    pub time: String,
    pub liters: f64,
    pub price_per_liter: f64,
    pub price: f64,
    pub fuel_rate: f64,
    pub odometer_before: i64,
    pub odometer_after: i64,
    pub method: String,
}

pub async fn fuel_events_page(
    pool: &PgPool,
    from: &str,
    to: &str,
    limit: i64,
    offset: i64,
) -> Result<(Vec<FullFuelEventRow>, i64)> {
    let total: i64 = sqlx::query(
        "SELECT COUNT(*) AS n FROM fuel_events \
         WHERE deleted_at IS NULL AND date BETWEEN $1 AND $2",
    )
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?
    .get("n");

    let rows = sqlx::query(
        "SELECT id, car_id, COALESCE(car_no_plate, '') AS car_no_plate, \
                COALESCE(driver_name, '') AS driver_name, \
                COALESCE(date, '') AS date, COALESCE(time, '') AS time, \
                COALESCE(liters, 0.0)::float8 AS liters, \
                COALESCE(price_per_liter, 0.0)::float8 AS price_per_liter, \
                COALESCE(price, 0.0)::float8 AS price, \
                COALESCE(fuel_rate, 0.0)::float8 AS fuel_rate, \
                COALESCE(odometer_before, 0)::bigint AS odometer_before, \
                COALESCE(odometer_after, 0)::bigint AS odometer_after, \
                COALESCE(NULLIF(method, ''), 'Manual') AS method \
         FROM fuel_events \
         WHERE deleted_at IS NULL AND date BETWEEN $1 AND $2 \
         ORDER BY date DESC, time DESC, id DESC LIMIT $3 OFFSET $4",
    )
    .bind(from)
    .bind(to)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|r| FullFuelEventRow {
            id: r.get::<i32, _>("id") as i64,
            car_id: r.get::<Option<i64>, _>("car_id"),
            car_no_plate: r.get("car_no_plate"),
            driver_name: r.get("driver_name"),
            date: r.get("date"),
            time: r.get("time"),
            liters: r.get("liters"),
            price_per_liter: r.get("price_per_liter"),
            price: r.get("price"),
            fuel_rate: r.get("fuel_rate"),
            odometer_before: r.get("odometer_before"),
            odometer_after: r.get("odometer_after"),
            method: r.get("method"),
        })
        .collect();
    Ok((items, total))
}

/* ------------------------------------------------------------------------ */
/* Attention — what expires or falls due next                                */
/* ------------------------------------------------------------------------ */

pub struct ExpiringDocRow {
    pub plate: String,
    pub kind: String,
    pub expires_on: String,
}

/// Vehicle documents already expired or expiring inside the horizon.
///
/// The three dates live in three columns on `cars` rather than a documents
/// table, so the union is the join — it reads every car three times, which at
/// fleet size is one index-free scan of twenty-odd rows.
///
/// They are stored as text. Lexical order is chronological for `YYYY-MM-DD`,
/// so the comparison and the sort are both plain string work — but only for
/// values that actually are dates, hence the shape guard: the legacy rows
/// carry `''` for "never recorded", and a malformed one must not sort itself
/// to the top of a list the fleet is meant to act on.
pub async fn expiring_documents(pool: &PgPool, horizon: &str) -> Result<Vec<ExpiringDocRow>> {
    let rows = sqlx::query(
        r#"
        SELECT plate, kind, expires_on FROM (
            SELECT car_no_plate AS plate, 'license'      AS kind, license_expiration_date      AS expires_on FROM cars WHERE deleted_at IS NULL
            UNION ALL
            SELECT car_no_plate,          'calibration',         calibration_expiration_date              FROM cars WHERE deleted_at IS NULL
            UNION ALL
            SELECT car_no_plate,          'tank_license',        tank_license_expiration_date             FROM cars WHERE deleted_at IS NULL
        ) d
        WHERE expires_on ~ '^\d{4}-\d{2}-\d{2}$'
          AND expires_on <= $1
        ORDER BY expires_on, plate
        "#,
    )
    .bind(horizon)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| ExpiringDocRow {
            plate: r.get::<Option<String>, _>("plate").unwrap_or_default(),
            kind: r.get("kind"),
            expires_on: r.get("expires_on"),
        })
        .collect())
}

pub struct OilChangeRow {
    pub plate: String,
    pub date: Option<String>,
    /// `mileage` is the service *interval* in km, not distance driven — the
    /// legacy column name the Go backend still writes.
    pub interval_km: f64,
    pub odometer_at_change: f64,
    pub current_odometer: f64,
    /// Which filters went in with the oil. Columns FalconGo added; older rows
    /// carry the DEFAULT false, which reads as "oil only".
    pub oil_filter: bool,
    pub fuel_filter: bool,
    pub water_filter: bool,
}

/// The most recent oil change per car.
///
/// `DISTINCT ON` descends the (car_id, date) order once per car rather than
/// grouping the whole table, the same shape as the fleet query above. Whether
/// a car is *due* is decided by the caller, because that rule lives in one
/// place shared with the oil-changes screen.
pub async fn latest_oil_change_per_car(pool: &PgPool) -> Result<Vec<OilChangeRow>> {
    let rows = sqlx::query(
        r#"
        SELECT DISTINCT ON (o.car_id)
               o.car_no_plate, o.date, o.mileage, o.odometer_at_change, o.current_odometer,
               COALESCE(o.oil_filter_changed, false)   AS oil_filter,
               COALESCE(o.fuel_filter_changed, false)  AS fuel_filter,
               COALESCE(o.water_filter_changed, false) AS water_filter
        FROM oil_changes o
        WHERE o.deleted_at IS NULL
        ORDER BY o.car_id, o.date DESC, o.id DESC
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| OilChangeRow {
            plate: r.get::<Option<String>, _>("car_no_plate").unwrap_or_default(),
            date: r.get("date"),
            interval_km: num(&r, "mileage"),
            odometer_at_change: num(&r, "odometer_at_change"),
            current_odometer: num(&r, "current_odometer"),
            oil_filter: r.get("oil_filter"),
            fuel_filter: r.get("fuel_filter"),
            water_filter: r.get("water_filter"),
        })
        .collect())
}

/// `numeric` columns arrive as `Decimal`; the arithmetic here is comparisons
/// against thresholds in whole kilometres, so f64 is honest for it.
fn num(row: &sqlx::postgres::PgRow, col: &str) -> f64 {
    row.get::<Option<Decimal>, _>(col)
        .and_then(|d| d.to_string().parse::<f64>().ok())
        .unwrap_or(0.0)
}
