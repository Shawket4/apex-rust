//! `GET /api/v1/dashboard` — the entry point's one payload, plus the four
//! drawers behind it.
//!
//! Design rules, in order of importance:
//!
//! * **One request paints the page.** Everything the first render needs is in
//!   the main payload; the drawers are separate endpoints fetched only when a
//!   card is opened. The live fleet status is deliberately NOT here — the
//!   browser takes that straight from etit-proxy, so neither service's latency
//!   or outage is the other's problem.
//! * **The queries run concurrently.** They are independent, so the endpoint
//!   costs its slowest member (~11 ms measured on production), not the sum.
//! * **Money is absent below permission 4, never zeroed.** A zero reads as
//!   "earned nothing"; absence reads as "not for you". The list at 1 keeps the
//!   fleet and trip counts, which is what a dispatcher needs from this page.
//! * **Money is decimal strings.** Same rule as every other endpoint here — a
//!   figure that has been through a JS float is a figure someone can be
//!   short-changed by.

use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use chrono::{Datelike, Utc};
use chrono_tz::Africa::Cairo;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::auth::Claims;
use crate::db::dashboard_queries as q;
use crate::db::stats_queries::get_stats_by_date;
use crate::utils::response;

/// The permission that may see money on the dashboard.
const FINANCIAL_PERMISSION: i32 = 4;

/* ------------------------------------------------------------------------ */
/* Wire shapes                                                               */
/* ------------------------------------------------------------------------ */

#[derive(Serialize)]
pub struct DashboardResponse {
    pub as_of: String,
    pub month: MonthBlock,
    /// Absent below permission 4.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub money: Option<MoneyBlock>,
    /// Absent below permission 4, like money.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fuel: Option<FuelBlock>,
    pub fleet: Vec<FleetEntry>,
    pub exceptions: Vec<Exception>,
}

#[derive(Serialize)]
pub struct MonthBlock {
    pub trips: i64,
    pub trucks: i64,
    pub litres: i64,
}

#[derive(Serialize)]
pub struct MoneyBlock {
    pub revenue: String,
    /// The same span of the previous month, so the delta compares like with
    /// like — 1–28 August against 1–28 July, never against July's full total.
    pub revenue_prev: String,
    /// Everything that left in the window: bank ledger + cash fuel +
    /// advances/loans issued. The components are also broken out so the card
    /// can show where the money went at a glance.
    pub cash_out: String,
    pub cash_out_bank: String,
    pub cash_out_fuel: String,
    pub cash_out_advances: String,
    /// Advances/loans issued in the window. Payroll recovers them against
    /// salaries outside this system, so the period view IS the useful
    /// figure (all-time `is_paid = false` is 19 months of already-recovered
    /// history).
    pub owed: OwedBlock,
    /// Top five categories (bank + the synthetic fuel/advances lines);
    /// everything smaller folds into "Other".
    pub by_category: Vec<CategoryOut>,
}

/// Who owes us money and in what form. `salary`-kind rows are excluded —
/// they are payroll visibility entries, not debt.
#[derive(Serialize, Default)]
pub struct OwedBlock {
    pub driver_advances: String,
    pub driver_advances_count: i64,
    pub driver_loans: String,
    pub driver_loans_count: i64,
    pub employee_advances: String,
    pub employee_advances_count: i64,
    pub employee_loans: String,
    pub employee_loans_count: i64,
    pub total: String,
}

/// The fuel card: today's consumption at a glance plus the latest events,
/// each linking to its detail page. Consumption view — every event counts
/// regardless of payment method (dedup against the bank is cash_out's job).
#[derive(Serialize)]
pub struct FuelBlock {
    pub today: String,
    pub today_liters: f64,
    pub today_events: i64,
    pub recent: Vec<FuelEventOut>,
}

#[derive(Serialize)]
pub struct FuelEventOut {
    pub id: i64,
    pub plate_no: String,
    pub plate_ar: String,
    pub driver_name: String,
    pub date: String,
    pub time: String,
    pub liters: f64,
    pub price_per_liter: String,
    pub price: String,
    pub method: String,
    pub fuel_rate: f64,
}

#[derive(Serialize)]
pub struct CategoryOut {
    pub key: String,
    pub out: String,
}

#[derive(Serialize)]
pub struct FleetEntry {
    /// `null` marks the untracked service vehicles — the frontend's one flag.
    pub etit_id: Option<String>,
    /// The digits — how the fleet actually refers to a truck.
    pub plate_no: String,
    /// The Arabic letters, secondary on the tile.
    pub plate_ar: String,
    pub last_trip_date: Option<String>,
    pub days_idle: Option<i64>,
    /// Revenue earned today / yesterday (Cairo days), company-filtered like
    /// the headline revenue. Absent below the financial permission.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revenue_today: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revenue_yesterday: Option<String>,
}

#[derive(Serialize)]
pub struct Exception {
    pub key: &'static str,
    pub severity: &'static str,
    pub count: i64,
    pub href: &'static str,
}

/* ------------------------------------------------------------------------ */
/* Month arithmetic                                                          */
/* ------------------------------------------------------------------------ */

/// The requested month's window, and the same span of the month before.
///
/// All boundaries are Cairo calendar days rendered as the `YYYY-MM-DD` strings
/// the trips table stores. For the current month the span runs to today; for a
/// past month it runs to the month's end — and the previous span is capped to
/// the same day-of-month (clamped to that month's length), so mid-month the
/// comparison never reads as a collapse.
struct Window {
    from: String,
    to: String,
    prev_from: String,
    prev_to: String,
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (ny, nm) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    chrono::NaiveDate::from_ymd_opt(ny, nm, 1)
        .unwrap()
        .pred_opt()
        .unwrap()
        .day()
}

/// An explicit `from..=to` window beats the month parameter. The previous
/// window is the same number of days ending the day before `from`, so the
/// delta stays like-for-like at any span length.
fn resolve_window(q: &DashboardQuery) -> Window {
    let parse = |s: &Option<String>| {
        s.as_deref()
            .and_then(|v| chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d").ok())
    };
    if let (Some(from), Some(to)) = (parse(&q.from), parse(&q.to)) {
        if from <= to {
            let len = (to - from).num_days();
            let prev_to = from - chrono::Duration::days(1);
            let prev_from = prev_to - chrono::Duration::days(len);
            return Window {
                from: from.format("%Y-%m-%d").to_string(),
                to: to.format("%Y-%m-%d").to_string(),
                prev_from: prev_from.format("%Y-%m-%d").to_string(),
                prev_to: prev_to.format("%Y-%m-%d").to_string(),
            };
        }
    }
    month_window(q.month.as_deref())
}

fn month_window(month: Option<&str>) -> Window {
    let today = Utc::now().with_timezone(&Cairo).date_naive();

    let (year, mon) = month
        .and_then(|m| {
            let mut it = m.splitn(2, '-');
            let y = it.next()?.parse::<i32>().ok()?;
            let mo = it.next()?.parse::<u32>().ok()?;
            (1..=12).contains(&mo).then_some((y, mo))
        })
        .unwrap_or((today.year(), today.month()));

    let is_current = year == today.year() && mon == today.month();
    let cap = if is_current { today.day() } else { days_in_month(year, mon) };

    let (py, pm) = if mon == 1 { (year - 1, 12) } else { (year, mon - 1) };
    let prev_cap = cap.min(days_in_month(py, pm));

    Window {
        from: format!("{year:04}-{mon:02}-01"),
        to: format!("{year:04}-{mon:02}-{cap:02}"),
        prev_from: format!("{py:04}-{pm:02}-01"),
        prev_to: format!("{py:04}-{pm:02}-{prev_cap:02}"),
    }
}

/* ------------------------------------------------------------------------ */
/* Plates                                                                    */
/* ------------------------------------------------------------------------ */

/// "ف ع ص 4381" → ("4381", "ف ع ص"). The digits are how people say a truck out
/// loud; the letters are the secondary line. The digits sit at either end
/// depending on who typed the plate in, so this splits by character class
/// rather than position. A plate with no digits keeps its whole text as the
/// number line rather than rendering blank.
fn split_plate(plate: &str) -> (String, String) {
    let digits: String = plate.chars().filter(|c| c.is_ascii_digit()).collect();
    let letters = plate
        .split_whitespace()
        .filter(|w| !w.chars().all(|c| c.is_ascii_digit()))
        .collect::<Vec<_>>()
        .join(" ");
    if digits.is_empty() {
        (plate.trim().to_string(), String::new())
    } else {
        (digits, letters)
    }
}

fn money_str(v: f64) -> String {
    format!("{v:.2}")
}

fn fuel_event_out(e: q::FuelEventRow) -> FuelEventOut {
    let (plate_no, plate_ar) = split_plate(&e.car_no_plate);
    FuelEventOut {
        id: e.id,
        plate_no,
        plate_ar,
        driver_name: e.driver_name,
        date: e.date,
        time: e.time,
        liters: e.liters,
        price_per_liter: money_str(e.price_per_liter),
        price: money_str(e.price),
        method: e.method,
        fuel_rate: e.fuel_rate,
    }
}

/* ------------------------------------------------------------------------ */
/* Handlers                                                                  */
/* ------------------------------------------------------------------------ */

#[derive(Deserialize)]
pub struct DashboardQuery {
    pub month: Option<String>,
    /// Explicit window (YYYY-MM-DD, inclusive). When both are valid they
    /// override `month`.
    pub from: Option<String>,
    pub to: Option<String>,
    /// Scopes the trips dimension (revenue, counts, fleet revenue). Cash-out
    /// and owed money have no company dimension and ignore it.
    pub company: Option<String>,
    pub format: Option<String>,
}

fn permission(req: &HttpRequest) -> i32 {
    req.extensions()
        .get::<Claims>()
        .and_then(|c| c.permission)
        .unwrap_or(0)
}

/// The route-level `required_permission` is NOT enforced by `JwtAuth` — the
/// middleware validates the token and admin user_type only, by design, so that
/// pages like statistics can serve reduced data to lower levels. Which means a
/// money-only endpoint must slam the door itself, here.
fn require_financial(req: &HttpRequest) -> Result<(), actix_web::Error> {
    if permission(req) >= FINANCIAL_PERMISSION {
        Ok(())
    } else {
        Err(actix_web::error::ErrorForbidden("financial access required"))
    }
}

fn internal(e: anyhow::Error) -> actix_web::Error {
    log::error!("dashboard query failed: {e:#}");
    actix_web::error::ErrorInternalServerError("dashboard unavailable")
}

pub async fn get_dashboard(
    pool: web::Data<PgPool>,
    query: web::Query<DashboardQuery>,
    req: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    let financial = permission(&req) >= FINANCIAL_PERMISSION;
    let use_msgpack = query.format.as_deref() == Some("msgpack");
    let w = resolve_window(&query);
    let company = query.company.as_deref().filter(|c| !c.trim().is_empty());
    let p = pool.get_ref();

    let today = Utc::now().with_timezone(&Cairo).date_naive();
    let today_s = today.format("%Y-%m-%d").to_string();
    let yesterday_s = (today - chrono::Duration::days(1)).format("%Y-%m-%d").to_string();

    // Everything the page needs, in flight together. The money queries only
    // join the party when the caller may see the results.
    let (totals, fleet, zero_trips, unreviewed) = tokio::try_join!(
        q::month_totals(p, &w.from, &w.to, company),
        q::fleet(p),
        q::trips_earning_zero(p, &w.from, &w.to, company),
        q::transactions_unreviewed(p),
    )
    .map_err(internal)?;

    let mut tile_revenue: std::collections::HashMap<(String, bool), f64> =
        std::collections::HashMap::new();

    let fuel = if financial {
        let (totals, recent) = tokio::try_join!(
            q::fuel_totals(p, &today_s, &today_s),
            q::recent_fuel_events(p, "0000-01-01", &today_s, 5),
        )
        .map_err(internal)?;
        Some(FuelBlock {
            today: money_str(totals.spend),
            today_liters: totals.liters,
            today_events: totals.events,
            recent: recent.into_iter().map(fuel_event_out).collect(),
        })
    } else {
        None
    };

    let money = if financial {
        let (revenue, revenue_prev, cash_bank, fuel_out, advances_out, categories, owed, per_car) =
            tokio::try_join!(
                q::revenue_total(p, &w.from, &w.to, company),
                q::revenue_total(p, &w.prev_from, &w.prev_to, company),
                q::cash_out_total(p, &w.from, &w.to),
                q::fuel_cash_out(p, &w.from, &w.to),
                q::advances_issued(p, &w.from, &w.to),
                q::cash_out_by_category(p, &w.from, &w.to),
                q::money_owed(p, &w.from, &w.to),
                q::fleet_revenue(p, &yesterday_s, &today_s, company),
            )
            .map_err(internal)?;

        for (plate, date, v) in per_car {
            tile_revenue.insert((plate, date == today_s), v);
        }

        // Bank categories plus the two flows the ledger never sees, ranked
        // together; the tail folds into Other so the panel has a fixed
        // height whatever the window holds.
        let mut all_cats: Vec<(String, f64)> = categories
            .iter()
            .map(|(k, v)| (k.clone(), v.to_string().parse::<f64>().unwrap_or(0.0)))
            .collect();
        if fuel_out > 0.0 {
            all_cats.push(("Fuel (cash)".into(), fuel_out));
        }
        if advances_out > 0.0 {
            all_cats.push(("Advances & loans".into(), advances_out));
        }
        all_cats.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let mut by_category: Vec<CategoryOut> = all_cats
            .iter()
            .take(5)
            .map(|(k, v)| CategoryOut { key: k.clone(), out: money_str(*v) })
            .collect();
        let tail: f64 = all_cats.iter().skip(5).map(|(_, v)| *v).sum();
        if tail > 0.0 {
            by_category.push(CategoryOut { key: "Other".into(), out: money_str(tail) });
        }

        let mut owed_block = OwedBlock::default();
        let mut owed_total = 0.0;
        for b in owed {
            owed_total += b.total;
            let (amount, count) = match (b.is_driver, b.kind.as_str()) {
                (true, "advance") => (&mut owed_block.driver_advances, &mut owed_block.driver_advances_count),
                (true, _)         => (&mut owed_block.driver_loans, &mut owed_block.driver_loans_count),
                (false, "advance") => (&mut owed_block.employee_advances, &mut owed_block.employee_advances_count),
                (false, _)        => (&mut owed_block.employee_loans, &mut owed_block.employee_loans_count),
            };
            *amount = money_str(b.total);
            *count = b.count;
        }
        for f in [
            &mut owed_block.driver_advances,
            &mut owed_block.driver_loans,
            &mut owed_block.employee_advances,
            &mut owed_block.employee_loans,
        ] {
            if f.is_empty() {
                *f = money_str(0.0);
            }
        }
        owed_block.total = money_str(owed_total);

        let cash_bank_f = cash_bank.to_string().parse::<f64>().unwrap_or(0.0);
        Some(MoneyBlock {
            revenue: money_str(revenue),
            revenue_prev: money_str(revenue_prev),
            cash_out: money_str(cash_bank_f + fuel_out + advances_out),
            cash_out_bank: money_str(cash_bank_f),
            cash_out_fuel: money_str(fuel_out),
            cash_out_advances: money_str(advances_out),
            owed: owed_block,
            by_category,
        })
    } else {
        None
    };

    let fleet = fleet
        .into_iter()
        .map(|c| {
            let (plate_no, plate_ar) = split_plate(&c.plate);
            let days_idle = c.last_trip_date.as_deref().and_then(|d| {
                chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d")
                    .ok()
                    .map(|last| (today - last).num_days().max(0))
            });
            let rev = |is_today: bool| {
                financial
                    .then(|| tile_revenue.get(&(c.plate.clone(), is_today)).copied().unwrap_or(0.0))
                    .map(money_str)
            };
            FleetEntry {
                etit_id: c.etit_id,
                revenue_today: rev(true),
                revenue_yesterday: rev(false),
                plate_no,
                plate_ar,
                last_trip_date: c.last_trip_date,
                days_idle,
            }
        })
        .collect();

    // Only exceptions that exist. An empty list is the good-day state, not a
    // rendering problem.
    let mut exceptions = Vec::new();
    if zero_trips > 0 {
        exceptions.push(Exception {
            key: "trips_earning_zero",
            severity: "warning",
            count: zero_trips,
            href: "/trips?md=a",
        });
    }
    if unreviewed > 0 {
        exceptions.push(Exception {
            key: "transactions_unreviewed",
            severity: "warning",
            count: unreviewed,
            href: "/fleet-expenses?uncat=1",
        });
    }

    let payload = DashboardResponse {
        as_of: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        month: MonthBlock {
            trips: totals.trips,
            trucks: totals.trucks,
            litres: totals.litres,
        },
        money,
        fuel,
        fleet,
        exceptions,
    };

    response(&payload, use_msgpack).map_err(actix_web::error::ErrorInternalServerError)
}

/* ------------------------------- drawers -------------------------------- */

#[derive(Serialize)]
struct RevenueDrawer {
    companies: Vec<NamedAmount>,
    daily: Vec<DailyAmount>,
}
#[derive(Serialize)]
struct NamedAmount {
    name: String,
    amount: String,
}
#[derive(Serialize)]
struct DailyAmount {
    date: String,
    amount: String,
}

pub async fn get_revenue_drawer(
    pool: web::Data<PgPool>,
    query: web::Query<DashboardQuery>,
    req: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    require_financial(&req)?;
    let use_msgpack = query.format.as_deref() == Some("msgpack");
    let w = resolve_window(&query);

    let company = query.company.as_deref().filter(|c| !c.trim().is_empty());
    let (companies, daily) = tokio::try_join!(
        q::revenue_by_company(pool.get_ref(), &w.from, &w.to, company),
        async {
            get_stats_by_date(pool.get_ref(), &w.from, &w.to, company, true).await
        },
    )
    .map_err(internal)?;

    let payload = RevenueDrawer {
        companies: companies
            .into_iter()
            .map(|(name, v)| NamedAmount { name, amount: money_str(v) })
            .collect(),
        daily: daily
            .into_iter()
            .map(|d| DailyAmount {
                date: d.date,
                amount: money_str(d.total_revenue.unwrap_or(0.0)),
            })
            .collect(),
    };
    response(&payload, use_msgpack).map_err(actix_web::error::ErrorInternalServerError)
}

#[derive(Serialize)]
struct CashOutDrawer {
    by_category: Vec<NamedAmount>,
    largest: Vec<PaymentRow>,
}
#[derive(Serialize)]
struct PaymentRow {
    occurred_at: String,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<String>,
    amount: String,
}

pub async fn get_cash_out_drawer(
    pool: web::Data<PgPool>,
    query: web::Query<DashboardQuery>,
    req: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    require_financial(&req)?;
    let use_msgpack = query.format.as_deref() == Some("msgpack");
    let w = resolve_window(&query);
    let p = pool.get_ref();

    let (categories, largest, fuel_out, advances_out) = tokio::try_join!(
        q::cash_out_by_category(p, &w.from, &w.to),
        q::largest_payments(p, &w.from, &w.to),
        q::fuel_cash_out(p, &w.from, &w.to),
        q::advances_issued(p, &w.from, &w.to),
    )
    .map_err(internal)?;

    let mut by_category: Vec<NamedAmount> = categories
        .into_iter()
        .map(|(name, v)| NamedAmount { name, amount: v.to_string() })
        .collect();
    if fuel_out > 0.0 {
        by_category.push(NamedAmount { name: "Fuel (cash)".into(), amount: money_str(fuel_out) });
    }
    if advances_out > 0.0 {
        by_category.push(NamedAmount {
            name: "Advances & loans".into(),
            amount: money_str(advances_out),
        });
    }
    by_category.sort_by(|a, b| {
        let av = a.amount.parse::<f64>().unwrap_or(0.0);
        let bv = b.amount.parse::<f64>().unwrap_or(0.0);
        bv.partial_cmp(&av).unwrap_or(std::cmp::Ordering::Equal)
    });

    let payload = CashOutDrawer {
        by_category,
        largest: largest
            .into_iter()
            .map(|x| PaymentRow {
                occurred_at: x.occurred_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                label: x.label,
                category: x.category,
                amount: x.amount.to_string(),
            })
            .collect(),
    };
    response(&payload, use_msgpack).map_err(actix_web::error::ErrorInternalServerError)
}

#[derive(Serialize)]
struct TripsDrawer {
    companies: Vec<NamedCount>,
    daily: Vec<DailyCount>,
}
#[derive(Serialize)]
struct NamedCount {
    name: String,
    trips: i64,
}
#[derive(Serialize)]
struct DailyCount {
    date: String,
    trips: i64,
}

pub async fn get_trips_drawer(
    pool: web::Data<PgPool>,
    query: web::Query<DashboardQuery>,
    _req: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    let use_msgpack = query.format.as_deref() == Some("msgpack");
    let w = resolve_window(&query);
    let p = pool.get_ref();

    let company = query.company.as_deref().filter(|c| !c.trim().is_empty());
    let (companies, daily) = tokio::try_join!(
        q::trips_by_company(p, &w.from, &w.to, company),
        q::trips_by_day(p, &w.from, &w.to, company),
    )
    .map_err(internal)?;

    let payload = TripsDrawer {
        companies: companies
            .into_iter()
            .map(|(name, trips)| NamedCount { name, trips })
            .collect(),
        daily: daily
            .into_iter()
            .map(|d| DailyCount { date: d.date, trips: d.trips })
            .collect(),
    };
    response(&payload, use_msgpack).map_err(actix_web::error::ErrorInternalServerError)
}

#[derive(Serialize)]
struct AdvancesDrawer {
    parties: Vec<PartyRow>,
}
#[derive(Serialize)]
struct PartyRow {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    /// "driver" or "employee" — who owes this line.
    audience: &'static str,
    total: String,
    count: i64,
}

pub async fn get_advances_drawer(
    pool: web::Data<PgPool>,
    query: web::Query<DashboardQuery>,
    req: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    require_financial(&req)?;
    let use_msgpack = query.format.as_deref() == Some("msgpack");
    let w = resolve_window(&query);
    let parties = q::advances_by_party(pool.get_ref(), &w.from, &w.to)
        .await
        .map_err(internal)?;

    let payload = AdvancesDrawer {
        parties: parties
            .into_iter()
            .map(|x| PartyRow {
                name: x.name,
                kind: x.kind,
                audience: if x.is_driver { "driver" } else { "employee" },
                total: money_str(x.total),
                count: x.count,
            })
            .collect(),
    };
    response(&payload, use_msgpack).map_err(actix_web::error::ErrorInternalServerError)
}

#[derive(Serialize)]
struct FuelDrawer {
    window_spend: String,
    window_liters: f64,
    window_events: i64,
    by_method: Vec<MethodOut>,
    events: Vec<FuelEventOut>,
}
#[derive(Serialize)]
struct MethodOut {
    method: String,
    spend: String,
    liters: f64,
}

pub async fn get_fuel_drawer(
    pool: web::Data<PgPool>,
    query: web::Query<DashboardQuery>,
    req: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    require_financial(&req)?;
    let use_msgpack = query.format.as_deref() == Some("msgpack");
    let w = resolve_window(&query);
    let p = pool.get_ref();

    let (totals, by_method, events) = tokio::try_join!(
        q::fuel_totals(p, &w.from, &w.to),
        q::fuel_by_method(p, &w.from, &w.to),
        q::recent_fuel_events(p, &w.from, &w.to, 25),
    )
    .map_err(internal)?;

    let payload = FuelDrawer {
        window_spend: money_str(totals.spend),
        window_liters: totals.liters,
        window_events: totals.events,
        by_method: by_method
            .into_iter()
            .map(|(method, spend, liters)| MethodOut {
                method,
                spend: money_str(spend),
                liters,
            })
            .collect(),
        events: events.into_iter().map(fuel_event_out).collect(),
    };
    response(&payload, use_msgpack).map_err(actix_web::error::ErrorInternalServerError)
}

/* ------------------------------------------------------------------------ */
/* Tests                                                                     */
/* ------------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plates_split_wherever_the_digits_sit() {
        assert_eq!(split_plate("ف ع ص 4381"), ("4381".into(), "ف ع ص".into()));
        assert_eq!(split_plate("5917 ف ج م"), ("5917".into(), "ف ج م".into()));
        // A plate that is only digits keeps them as the number line.
        assert_eq!(split_plate("3619"), ("3619".into(), "".into()));
        // No digits at all: whole text survives rather than a blank tile.
        assert_eq!(split_plate("بدون"), ("بدون".into(), "".into()));
    }

    #[test]
    fn windows_compare_like_with_like() {
        // A past month runs full-length and compares against the whole
        // previous month, clamped to its length.
        let w = month_window(Some("2026-03"));
        assert_eq!(w.from, "2026-03-01");
        assert_eq!(w.to, "2026-03-31");
        assert_eq!(w.prev_from, "2026-02-01");
        assert_eq!(w.prev_to, "2026-02-28");

        // January's previous month crosses the year.
        let w = month_window(Some("2026-01"));
        assert_eq!(w.prev_from, "2025-12-01");
        assert_eq!(w.prev_to, "2025-12-31");

        // Garbage falls back to the current month rather than erroring the
        // entry point.
        let w = month_window(Some("not-a-month"));
        assert_eq!(&w.from[8..], "01");
    }

    #[test]
    fn explicit_window_beats_month_and_mirrors_backwards() {
        let q = |from: &str, to: &str| DashboardQuery {
            month: Some("2026-01".into()),
            from: Some(from.into()),
            to: Some(to.into()),
            company: None,
            format: None,
        };

        // A 7-day scope compares against the 7 days right before it.
        let w = resolve_window(&q("2026-08-22", "2026-08-28"));
        assert_eq!((w.from.as_str(), w.to.as_str()), ("2026-08-22", "2026-08-28"));
        assert_eq!(
            (w.prev_from.as_str(), w.prev_to.as_str()),
            ("2026-08-15", "2026-08-21")
        );

        // A single day compares against yesterday, across a month boundary.
        let w = resolve_window(&q("2026-09-01", "2026-09-01"));
        assert_eq!(
            (w.prev_from.as_str(), w.prev_to.as_str()),
            ("2026-08-31", "2026-08-31")
        );

        // Inverted or malformed ranges fall back to the month parameter.
        let w = resolve_window(&q("2026-08-28", "2026-08-22"));
        assert_eq!(w.from, "2026-01-01");
        let w = resolve_window(&q("garbage", "2026-08-28"));
        assert_eq!(w.from, "2026-01-01");
    }
}
