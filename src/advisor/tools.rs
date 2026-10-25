//! The advisor's tools: what Claude can look at, and the one thing it can do
//! (propose a change, in any of the fleet's systems, for an admin to approve).
//!
//! Curated tools cover the questions the owner asked for — where a truck went,
//! what each truck/company/terminal earns, where it could have earned more,
//! what maintenance is due, what the service history says — and `run_sql` is
//! the read-only escape hatch for everything else.

use std::sync::Mutex;

use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::llm::ToolDef;
use super::upstream::{self, BearerToken};
use super::{actions, falcon_actions, fleet, search, sql, store, Advisor};

/// Everything a tool may touch during one turn.
pub struct ToolCtx<'a> {
    pub pool: &'a PgPool,
    pub adv: &'a Advisor,
    pub token: &'a BearerToken,
    pub user_id: i64,
    pub conversation_id: Option<Uuid>,
    /// Suggestions proposed during this turn, returned with the answer so
    /// the client can render their approve buttons.
    pub proposed: Mutex<Vec<Uuid>>,
    /// Progress for the chat stream: one event per tool as it starts.
    pub events: Option<tokio::sync::mpsc::UnboundedSender<Value>>,
}

pub fn tool_defs() -> Vec<ToolDef> {
    let date = json!({ "type": "string", "description": "YYYY-MM-DD" });
    vec![
        ToolDef {
            name: "find_truck",
            description: "Find trucks by plate (Arabic or Latin, spaces ignored), car id, or driver name. Returns car id, plate, operating company/area, driver, and the ETIT tracker id. Use it first whenever the user names a truck.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }),
        },
        ToolDef {
            name: "fleet_status",
            description: "Where every truck is NOW and what it is doing, from the live trackers (ETIT via etit-proxy, refreshed every minute): status (moving / idling / stopped / offline / no_signal when the last fix is over 30 min old / geofence_violation), speed, last fix time (Cairo), the terminal or drop-off point it is at (by the same geofences the trip audit uses), whether it is inside a dead zone, and the driver logged in on its tablet. Use this — never cars.latitude/longitude/slack_status, which are a year stale — for any question about current location, status or signal.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "car_id": { "type": "integer" },
                    "company": { "type": "string", "description": "only trucks assigned to this operating company" },
                    "status": { "type": "string", "enum": ["moving", "idling", "stopped", "offline", "no_signal", "geofence_violation", "unknown", "in_dead_zone"] }
                }
            }),
        },
        ToolDef {
            name: "dead_zones",
            description: "Dead zones (المناطق الميتة / المناطق المحظورة) are restricted areas trucks must not enter: circles an operator defines on the Dead Zones page. A violation is a stay of 2+ minutes inside one. Returns the active zones and which trucks are inside one right now; with from/to (Cairo days, max 92 days) it also lists every violation in that range from the GPS history (a report-only scan — nobody is alerted). This is NOT about lost GPS signal; for that use fleet_status (no_signal) or truck_itinerary (disconnected time).".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "from": date.clone(), "to": date.clone(),
                    "car_id": { "type": "integer", "description": "only this truck's violations" }
                }
            }),
        },
        ToolDef {
            name: "fleet_overview",
            description: "Every active truck with its operating company and area, driver, last trip date, and over the last N days: trips, base revenue (the trips' stored revenue before VAT), rental (its share of the truck-month rental TAQA/Petromin pay, spread over the days it worked), VAT, total (base + rental + VAT) and unpriced trips; plus this month's stored truck-month rental so far. Per-company totals too. The starting point for utilisation and rebalancing questions.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "days": { "type": "integer", "minimum": 1, "maximum": 120, "description": "Look-back window, default 30" },
                    "company": { "type": "string", "description": "Only this operating company" }
                }
            }),
        },
        ToolDef {
            name: "revenue_breakdown",
            description: "Stored revenue between two dates, grouped by car, company, terminal, drop_off_point, driver or day, from FalconGo's priced trips: base_revenue (before VAT), rental (each trip's share of its truck-month rental: a car-month's rental spread evenly over the days the car worked, a day split between that day's trips — so a part-month gets the worked days' share), vat, total_revenue (base + rental + VAT), trip count, unpriced trips (no pricing rule/fee mapping/band rate — they earn 0 until fixed) and mileage. Grouped by car it also subtracts fuel, oil-change and service costs: margin_before_driver = total_revenue − fuel − oil − service (driver pay excluded).".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "from": date.clone(), "to": date.clone(),
                    "group_by": { "type": "string", "enum": ["car", "company", "terminal", "drop_off_point", "driver", "day"] },
                    "company": { "type": "string" },
                    "car_id": { "type": "integer" }
                },
                "required": ["from", "to", "group_by"]
            }),
        },
        ToolDef {
            name: "pricing_rules",
            description: "What each customer company is charged: its pricing versions from FalconGo's pricing rules, effective-dated — the one in force on a date (default today), later (future) versions and the history. Each has its method (per_1000l_fee: tank litres × the route's fee / 1000, Petrol Arrows; band_per_1000l: tank litres × the band's rate / 1000, Watanya, with its band table; per_km: rate × the trip's furthest drop km, TAQA/Petromin), VAT rate and car rental (monthly_taper: per car-month, tapered below rental_full_days; per_car_day). Use it to answer \"what do we charge Watanya for band 5\" and before proposing create_pricing_rule.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "company": { "type": "string" },
                    "on_date": { "type": "string", "description": "YYYY-MM-DD; which version is in force on this date (default today, Cairo)" },
                    "history": { "type": "boolean", "description": "include superseded versions (default true)" }
                }
            }),
        },
        ToolDef {
            name: "truck_itinerary",
            description: "Where a truck actually went, from its GPS tracker: stops (place, arrival, departure, dwell), terminal/drop-off visits matched to trips, trip legs with actual km, and the period's mileage and active/idle time. Up to a few days at a time.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "car_id": { "type": "integer" },
                    "from": { "type": "string", "description": "YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS (Cairo time)" },
                    "to": { "type": "string", "description": "Same format; omit for a single day" }
                },
                "required": ["car_id", "from"]
            }),
        },
        ToolDef {
            name: "trip_audit",
            description: "The GPS trip audit: for each registered trip, did the truck really go terminal → drop-offs → back, actual vs expected km, efficiency, flags (detours, missed drop-offs, unmatched trips) and the worst routes.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "from": date, "to": date,
                    "company": { "type": "string" },
                    "flagged_only": { "type": "boolean" }
                },
                "required": ["from", "to"]
            }),
        },
        ToolDef {
            name: "route_options",
            description: "Where a truck could earn the most: every (terminal, drop-off) route of a company with its expected revenue per delivery for THIS truck's tank — priced by FalconGo's pricing engine on the company's rule in force today (Petrol Arrows: route fee × litres / 1000; Watanya: band rate × litres / 1000; TAQA/Petromin: per-km rate × distance) — its VAT, distance, and the empty km from the truck's current position to the terminal (road distance when available). Ranked by base revenue per total km (empty run + there and back). Car rental is per truck-month, not per delivery, so it is not in the per-route figures.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "car_id": { "type": "integer" },
                    "company": { "type": "string", "description": "Customer company; defaults to the truck's operating company" },
                    "terminal": { "type": "string" }
                },
                "required": ["car_id"]
            }),
        },
        ToolDef {
            name: "search_service_history",
            description: "Search the workshop service records (inspection items on service invoices). Give several variants of what a matching record might literally say — Egyptian Arabic, slang, the specific parts, the English part name (e.g. فرامل → تيل، دسك، هوبة، brake pads). Results are fused across variants.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "variants": { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": search::MAX_VARIANTS },
                    "car_id": { "type": "integer" },
                    "since": date,
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50 }
                },
                "required": ["variants"]
            }),
        },
        ToolDef {
            name: "maintenance_status",
            description: "From the maintenance system: service plans that are overdue / due soon / never done per truck (with current odometer), and open work orders.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "car_id": { "type": "integer" } }
            }),
        },
        ToolDef {
            name: "run_sql",
            description: format!(
                "One read-only SELECT on the apex database (schema in your instructions). At most {} rows come back; aggregate in SQL. Use when no other tool fits.",
                sql::MAX_ROWS
            ),
            parameters: json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "describe_actions",
            description: "The exact parameters of one or more change kinds, before proposing them. Call it for any kind you have not used in this conversation.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "kinds": { "type": "array", "items": { "type": "string", "enum": actions::kind_names() } }
                },
                "required": ["kinds"]
            }),
        },
        ToolDef {
            name: "propose_change",
            description: format!(
                "Propose ONE change for an admin to approve: creating, editing or deleting a record in FalconGo (trips, fuel, oil changes, service invoices, trucks, drivers, driver money, vendors, prices, violations), maint-rust (service records, oil changes, odometer, tires, schedules, stock) or the bank-SMS ledger. Nothing changes until an admin with permission 4 approves it; then it is applied through that system's own API in their name, marked as coming from this chat. Check the record first (find_truck, run_sql) and use real ids. Only propose what the data supports, and say why. Kinds:\n{}",
                actions::summary()
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": actions::kind_names() },
                    "params": { "type": "object", "description": "as describe_actions lists them" },
                    "title": { "type": "string", "description": "One line, in the user's language" },
                    "rationale": { "type": "string", "description": "The evidence, with numbers" }
                },
                "required": ["kind", "params", "title", "rationale"]
            }),
        },
    ]
}

/// Run one tool. Failures come back as `{"error": …}` for the model to read.
pub async fn run(ctx: &ToolCtx<'_>, name: &str, args: &Value) -> Value {
    let result = match name {
        "find_truck" => find_truck(ctx, args).await,
        "fleet_overview" => fleet_overview(ctx, args).await,
        "fleet_status" => fleet::fleet_status(ctx, args).await,
        "dead_zones" => fleet::dead_zones(ctx, args).await,
        "revenue_breakdown" => revenue_breakdown(ctx, args).await,
        "truck_itinerary" => truck_itinerary(ctx, args).await,
        "trip_audit" => trip_audit(ctx, args).await,
        "route_options" => route_options(ctx, args).await,
        "pricing_rules" => pricing_rules(ctx, args).await,
        "search_service_history" => search_history(ctx, args).await,
        "maintenance_status" => maintenance_status(ctx, args).await,
        "run_sql" => {
            let q = args.get("query").and_then(Value::as_str).unwrap_or("");
            Ok(sql::run(ctx.pool, q).await)
        }
        "describe_actions" => {
            let kinds: Vec<String> = args
                .get("kinds")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|k| k.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            Ok(actions::describe(&kinds))
        }
        "propose_change" => propose_change(ctx, args).await,
        other => Err(format!("unknown tool `{other}`")),
    };
    result.unwrap_or_else(|e| json!({ "error": e }))
}

type ToolResult = Result<Value, String>;

fn s<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn i(args: &Value, key: &str) -> Option<i64> {
    args.get(key)
        .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
}

fn date(args: &Value, key: &str) -> Result<String, String> {
    let v = s(args, key).ok_or_else(|| format!("`{key}` is required (YYYY-MM-DD)"))?;
    chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d")
        .map(|d| d.format("%Y-%m-%d").to_string())
        .map_err(|_| format!("`{key}` must be YYYY-MM-DD, got {v}"))
}

/// Rows of a query as a JSON array, via jsonb_agg so any column set works.
async fn rows(pool: &PgPool, query: &str, binds: Vec<Value>) -> Result<Value, String> {
    let wrapped = format!("SELECT COALESCE(jsonb_agg(t), '[]'::jsonb) FROM ({query}) t");
    let mut q = sqlx::query_as::<_, (Value,)>(&wrapped);
    for b in binds {
        q = match b {
            Value::Null => q.bind(None::<String>),
            Value::Number(n) if n.is_i64() => q.bind(n.as_i64()),
            Value::Number(n) => q.bind(n.as_f64()),
            Value::String(s) => q.bind(s),
            Value::Bool(b) => q.bind(b),
            other => q.bind(other),
        };
    }
    q.fetch_one(pool)
        .await
        .map(|(v,)| v)
        .map_err(|e| format!("query failed: {e}"))
}

async fn find_truck(ctx: &ToolCtx<'_>, args: &Value) -> ToolResult {
    let text = s(args, "text").ok_or("`text` is required")?;
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    rows(
        ctx.pool,
        "SELECT c.id AS car_id, c.car_no_plate, c.car_type, c.operating_company, c.operating_area,
                c.driver_id, d.name AS driver_name, NULLIF(c.etit_car_id, '') AS etit_car_id,
                c.tank_capacity
           FROM cars c
           LEFT JOIN drivers d ON d.id = c.driver_id
          WHERE c.deleted_at IS NULL
            AND (replace(c.car_no_plate, ' ', '') ILIKE '%' || $1 || '%'
                 OR c.id::text = $2
                 OR d.name ILIKE '%' || $2 || '%')
          ORDER BY c.car_no_plate LIMIT 20",
        vec![json!(compact), json!(text)],
    )
    .await
}

/// What every money figure in these tools means, for the model to quote.
const MONEY_BASIS: &str = "base_revenue = the trips' stored revenue before VAT (FalconGo prices each trip on write); rental = the trips' share of their truck-month rental (a car-month's stored rental spread evenly over the days the car worked); vat = VAT on both; total_revenue = base + rental + VAT. EGP.";

async fn fleet_overview(ctx: &ToolCtx<'_>, args: &Value) -> ToolResult {
    let days = i(args, "days").unwrap_or(30).clamp(1, 120);
    let company = s(args, "company").map(str::to_string);
    // Per car over the window, from the stored revenue; and the car's stored
    // truck-month rental for the current month (recomputed nightly).
    let per_car = "
        r AS (
            SELECT t.car_id, count(*) AS trips,
                   sum(COALESCE(tr.base_revenue, 0)) AS base,
                   sum(tr.allocated_rental) AS rental,
                   sum(tr.allocated_vat) AS vat,
                   sum(tr.allocated_total) AS total,
                   count(*) FILTER (WHERE tr.base_revenue IS NULL) AS unpriced,
                   max(t.date) AS last_trip_date
              FROM trip_revenue tr
              JOIN trips t ON t.id = tr.trip_id
             WHERE tr.date >= to_char(current_date - $1::int, 'YYYY-MM-DD')
             GROUP BY t.car_id),
        m AS (
            SELECT car_no_plate, sum(rental) AS month_rental, max(month_days_worked) AS month_days
              FROM truck_month_charges
             WHERE month = date_trunc('month', (now() AT TIME ZONE 'Africa/Cairo'))::date
             GROUP BY car_no_plate)";
    let trucks = rows(
        ctx.pool,
        &format!(
            "WITH {per_car}
             SELECT c.id AS car_id, c.car_no_plate, c.operating_company, c.operating_area,
                    d.name AS driver_name,
                    COALESCE(r.trips, 0) AS trips,
                    round(COALESCE(r.base, 0)::numeric, 0) AS base_revenue,
                    round(COALESCE(r.rental, 0)::numeric, 0) AS rental,
                    round(COALESCE(r.vat, 0)::numeric, 0) AS vat,
                    round(COALESCE(r.total, 0)::numeric, 0) AS total_revenue,
                    COALESCE(r.unpriced, 0) AS unpriced_trips,
                    r.last_trip_date,
                    round(m.month_rental::numeric, 0) AS this_month_rental_stored,
                    m.month_days AS this_month_days_worked
               FROM cars c
               LEFT JOIN drivers d ON d.id = c.driver_id
               LEFT JOIN r ON r.car_id = c.id
               LEFT JOIN m ON m.car_no_plate = c.car_no_plate
              WHERE c.deleted_at IS NULL
                AND ($2::text IS NULL OR c.operating_company = $2)
              ORDER BY c.operating_company, total_revenue DESC"
        ),
        vec![json!(days), json!(company)],
    )
    .await?;
    let companies = rows(
        ctx.pool,
        &format!(
            "WITH {per_car}
             SELECT COALESCE(NULLIF(c.operating_company, ''), '(none)') AS operating_company,
                    count(*) AS trucks,
                    count(*) FILTER (WHERE COALESCE(r.trips, 0) = 0) AS idle_trucks,
                    COALESCE(sum(r.trips), 0) AS trips,
                    round(COALESCE(sum(r.base), 0)::numeric, 0) AS base_revenue,
                    round(COALESCE(sum(r.rental), 0)::numeric, 0) AS rental,
                    round(COALESCE(sum(r.vat), 0)::numeric, 0) AS vat,
                    round(COALESCE(sum(r.total), 0)::numeric, 0) AS total_revenue,
                    round((COALESCE(sum(r.total), 0) / NULLIF(count(*), 0))::numeric, 0) AS total_per_truck,
                    COALESCE(sum(r.unpriced), 0) AS unpriced_trips,
                    round(COALESCE(sum(m.month_rental), 0)::numeric, 0) AS this_month_rental_stored
               FROM cars c
               LEFT JOIN r ON r.car_id = c.id
               LEFT JOIN m ON m.car_no_plate = c.car_no_plate
              WHERE c.deleted_at IS NULL
                AND ($2::text IS NULL OR c.operating_company = $2)
              GROUP BY 1 ORDER BY total_revenue DESC"
        ),
        vec![json!(days), json!(s(args, "company"))],
    )
    .await?;
    Ok(json!({
        "window_days": days,
        "money_basis": MONEY_BASIS,
        "companies": companies,
        "trucks": trucks,
    }))
}

async fn revenue_breakdown(ctx: &ToolCtx<'_>, args: &Value) -> ToolResult {
    let from = date(args, "from")?;
    let to = date(args, "to")?;
    let group = s(args, "group_by").unwrap_or("car");
    let key = match group {
        "car" => "t.car_id::text",
        "company" => "t.company",
        "terminal" => "t.terminal",
        "drop_off_point" => "t.drop_off_point",
        "driver" => "t.driver_name",
        "day" => "t.date",
        other => return Err(format!("unknown group_by `{other}`")),
    };
    // The stored revenue, per row, filtered on the view's own company/date so
    // the predicate reaches below its window.
    let base = format!(
        "SELECT {key} AS key, min(t.car_no_plate) AS plate_example,
                count(*) AS trips,
                round(sum(COALESCE(tr.base_revenue, 0))::numeric, 0) AS base_revenue,
                round(sum(tr.allocated_rental)::numeric, 0) AS rental,
                round(sum(tr.allocated_vat)::numeric, 0) AS vat,
                round(sum(tr.allocated_total)::numeric, 0) AS total_revenue,
                count(*) FILTER (WHERE tr.base_revenue IS NULL) AS unpriced_trips,
                round(sum(t.mileage)::numeric, 0) AS mileage_km,
                round((sum(tr.allocated_total) / NULLIF(count(*), 0))::numeric, 0) AS total_per_trip
           FROM trip_revenue tr
           JOIN trips t ON t.id = tr.trip_id
          WHERE tr.date BETWEEN $1 AND $2
            AND ($3::text IS NULL OR tr.company = $3)
            AND ($4::bigint IS NULL OR t.car_id = $4::bigint)
          GROUP BY 1"
    );
    let query = if group == "car" {
        // Per-truck margin: the period's fuel, oil and service spend.
        format!(
            "WITH r AS ({base}),
             f AS (SELECT car_id::text AS key, sum(price) AS fuel FROM fuel_events
                    WHERE deleted_at IS NULL AND date BETWEEN $1 AND $2 GROUP BY 1),
             o AS (SELECT car_id::text AS key, sum(cost) AS oil FROM oil_changes
                    WHERE deleted_at IS NULL AND date BETWEEN $1 AND $2 GROUP BY 1),
             sv AS (SELECT car_id::text AS key, sum(cost) AS service FROM services
                    WHERE deleted_at IS NULL AND date_of_service BETWEEN $1 AND $2 GROUP BY 1)
             SELECT r.*, c.car_no_plate, c.operating_company,
                    round(COALESCE(f.fuel, 0)::numeric, 0) AS fuel_cost,
                    round(COALESCE(o.oil, 0)::numeric, 0) AS oil_cost,
                    round(COALESCE(sv.service, 0)::numeric, 0) AS service_cost,
                    round((r.total_revenue - COALESCE(f.fuel, 0) - COALESCE(o.oil, 0) - COALESCE(sv.service, 0))::numeric, 0) AS margin_before_driver
               FROM r
               LEFT JOIN cars c ON c.id::text = r.key
               LEFT JOIN f ON f.key = r.key
               LEFT JOIN o ON o.key = r.key
               LEFT JOIN sv ON sv.key = r.key
              ORDER BY margin_before_driver DESC"
        )
    } else {
        format!("{base} ORDER BY total_revenue DESC")
    };
    let out = rows(
        ctx.pool,
        &query,
        vec![json!(from), json!(to), json!(s(args, "company")), json!(i(args, "car_id"))],
    )
    .await?;
    let mut result = json!({ "from": from, "to": to, "group_by": group, "money_basis": MONEY_BASIS, "rows": out });
    if group == "car" {
        result["margin_basis"] = json!(
            "margin_before_driver = total_revenue (base + rental + VAT) − fuel − oil changes − services in the period; driver pay is not subtracted"
        );
    }
    Ok(result)
}

/// `pricing_rules`: each company's effective-dated versions, split into the one
/// in force on a date, the later ones, and the history.
async fn pricing_rules(ctx: &ToolCtx<'_>, args: &Value) -> ToolResult {
    let on = match s(args, "on_date") {
        Some(_) => date(args, "on_date")?,
        None => chrono::Utc::now().with_timezone(&chrono_tz::Africa::Cairo).format("%Y-%m-%d").to_string(),
    };
    let history = args.get("history").and_then(Value::as_bool).unwrap_or(true);
    let all = rows(
        ctx.pool,
        "SELECT r.id, r.company, to_char(r.effective_from, 'YYYY-MM-DD') AS effective_from,
                r.method, r.per_km_rate::float8 AS per_km_rate, r.vat_rate::float8 AS vat_rate,
                r.rental_mode, r.rental_amount::float8 AS rental_amount, r.rental_full_days,
                r.notes, r.created_by_user_id, to_char(r.created_at, 'YYYY-MM-DD') AS created_on,
                (SELECT COALESCE(jsonb_agg(jsonb_build_object('band', b.band,
                                 'rate_per_1000l', b.rate_per_1000l::float8) ORDER BY b.band), '[]'::jsonb)
                   FROM pricing_bands b WHERE b.rule_id = r.id) AS bands
           FROM pricing_rules r
          WHERE ($1::text IS NULL OR r.company = $1)
          ORDER BY r.company, r.effective_from DESC",
        vec![json!(s(args, "company"))],
    )
    .await?;
    let mut companies: Vec<Value> = Vec::new();
    for rule in all.as_array().into_iter().flatten() {
        let company = rule["company"].as_str().unwrap_or_default();
        if companies.last().map(|c| c["company"] != company).unwrap_or(true) {
            companies.push(json!({ "company": company, "in_force": null, "future": [], "history": [] }));
        }
        let entry = companies.last_mut().expect("just pushed");
        let starts = rule["effective_from"].as_str().unwrap_or_default();
        // Newest first: the first version on or before the date is in force.
        if starts > on.as_str() {
            entry["future"].as_array_mut().expect("array").push(rule.clone());
        } else if entry["in_force"].is_null() {
            entry["in_force"] = rule.clone();
        } else if history {
            entry["history"].as_array_mut().expect("array").push(rule.clone());
        }
    }
    if companies.is_empty() {
        return Err(match s(args, "company") {
            Some(c) => format!("no pricing rules for `{c}`; companies are named as on trips (Petrol Arrows, Watanya, TAQA, Petromin)"),
            None => "no pricing rules exist yet".into(),
        });
    }
    Ok(json!({
        "on_date": on,
        "how_to_read": "per_1000l_fee: base = tank litres × the route's fee_mappings.fee / 1000. band_per_1000l: base = tank litres × bands[fee_mappings.fee].rate_per_1000l / 1000. per_km: base = per_km_rate × the trip's furthest drop km. VAT = vat_rate × base (and × rental). monthly_taper: rental_amount per car-month at rental_full_days or more days worked, less rental_amount/rental_full_days per day short. per_car_day: rental_amount per distinct car-day. A trip is priced by the version in force on its date; rental by the version in force on its month's first day.",
        "companies": companies,
    }))
}

/// The car's ETIT tracker id, or a message the model can act on.
async fn etit_id(pool: &PgPool, car_id: i64) -> Result<(String, String), String> {
    let row: Option<(Option<String>, String)> = sqlx::query_as(
        "SELECT etit_car_id, car_no_plate FROM cars WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(car_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?;
    match row {
        None => Err(format!("no car with id {car_id}")),
        Some((None, plate)) => Err(format!("{plate} has no ETIT tracker linked")),
        Some((Some(id), _)) if id.trim().is_empty() => Err(format!("car {car_id} has no ETIT tracker linked")),
        Some((Some(id), plate)) => Ok((id, plate)),
    }
}

fn range_query(args: &Value) -> Result<Vec<(&'static str, String)>, String> {
    let from = s(args, "from").ok_or("`from` is required")?;
    Ok(match s(args, "to") {
        None if from.len() == 10 => vec![("date", from.to_string())],
        None => return Err("`to` is required with a time".into()),
        Some(to) => {
            let start = if from.len() == 10 { format!("{from}T00:00:00") } else { from.to_string() };
            let end = if to.len() == 10 { format!("{to}T23:59:59") } else { to.to_string() };
            vec![("from", start), ("to", end)]
        }
    })
}

/// Keep what answers "where did it go"; drop the raw point stream.
fn condense_history(h: &Value) -> Value {
    let take = |key: &str, n: usize| -> Value {
        h.get(key)
            .and_then(Value::as_array)
            .map(|a| Value::Array(a.iter().take(n).cloned().collect()))
            .unwrap_or_else(|| json!([]))
    };
    json!({
        "stops": take("stops", 80),
        "pins": take("pins", 80),
        "legs": take("legs", 40),
        "stop_count": h.get("stops").and_then(Value::as_array).map(|a| a.len()),
    })
}

async fn truck_itinerary(ctx: &ToolCtx<'_>, args: &Value) -> ToolResult {
    let car_id = i(args, "car_id").ok_or("`car_id` is required")?;
    let (vid, plate) = etit_id(ctx.pool, car_id).await?;
    let query = range_query(args)?;
    let a = &ctx.adv;
    let history = upstream::etit_get(&a.http, &a.cfg, ctx.token, &format!("/vehicles/{vid}/history"), &query)
        .await
        .map_err(|e| e.to_string())?;
    let summary = upstream::etit_get(&a.http, &a.cfg, ctx.token, &format!("/vehicles/{vid}/history/summary"), &query)
        .await
        .ok();
    Ok(json!({
        "car_id": car_id, "plate": plate,
        "summary": summary,
        "itinerary": condense_history(&history),
    }))
}

async fn trip_audit(ctx: &ToolCtx<'_>, args: &Value) -> ToolResult {
    let from = date(args, "from")?;
    let to = date(args, "to")?;
    let mut q = vec![("from", from.clone()), ("to", to.clone())];
    if let Some(c) = s(args, "company") {
        q.push(("company", c.to_string()));
    }
    let a = &ctx.adv;
    let summary = upstream::etit_get(&a.http, &a.cfg, ctx.token, "/trip-audit/summary", &q)
        .await
        .map_err(|e| e.to_string())?;
    let mut mq = q.clone();
    mq.push(("per_page", "25".into()));
    mq.push(("sort", "severity".into()));
    if args.get("flagged_only").and_then(Value::as_bool).unwrap_or(true) {
        mq.push(("flagged", "true".into()));
    }
    let matches = upstream::etit_get(&a.http, &a.cfg, ctx.token, "/trip-audit/matches", &mq)
        .await
        .unwrap_or_else(|e| json!({ "error": e.to_string() }));
    Ok(json!({ "summary": summary, "matches": matches }))
}

fn haversine_km(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (lat1, lon1) = (a.0.to_radians(), a.1.to_radians());
    let (lat2, lon2) = (b.0.to_radians(), b.1.to_radians());
    let h = ((lat2 - lat1) / 2.0).sin().powi(2)
        + lat1.cos() * lat2.cos() * ((lon2 - lon1) / 2.0).sin().powi(2);
    2.0 * 6371.0 * h.sqrt().asin()
}

async fn route_options(ctx: &ToolCtx<'_>, args: &Value) -> ToolResult {
    let car_id = i(args, "car_id").ok_or("`car_id` is required")?;
    // The tank the routes are priced for: the truck's, or what it last carried.
    let car: Option<(String, Option<String>, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT c.car_no_plate, c.operating_company, NULLIF(c.tank_capacity, 0)::bigint,
                (SELECT NULLIF(t.tank_capacity, 0)::bigint FROM trips t
                  WHERE t.car_id = c.id AND t.deleted_at IS NULL ORDER BY t.date DESC, t.id DESC LIMIT 1)
           FROM cars c WHERE c.id = $1 AND c.deleted_at IS NULL",
    )
    .bind(car_id)
    .fetch_optional(ctx.pool)
    .await
    .map_err(|e| e.to_string())?;
    let (plate, op_company, car_tank, last_tank) = car.ok_or(format!("no car with id {car_id}"))?;
    let (tank, tank_source) = match (car_tank, last_tank) {
        (Some(t), _) => (Some(t), "the truck's tank_capacity"),
        (None, Some(t)) => (Some(t), "its most recent trip's tank_capacity"),
        (None, None) => (None, "unknown"),
    };
    let company = s(args, "company")
        .map(str::to_string)
        .or(op_company.filter(|c| !c.trim().is_empty()))
        .ok_or("the truck has no operating company; pass `company`")?;

    // The company's pricing version in force today (Cairo).
    #[derive(sqlx::FromRow)]
    struct Rule {
        id: i64,
        effective_from: String,
        method: String,
        per_km_rate: Option<f64>,
        vat_rate: f64,
        rental_mode: String,
        rental_amount: Option<f64>,
    }
    let rule: Rule = sqlx::query_as(
        "SELECT id, to_char(effective_from, 'YYYY-MM-DD') AS effective_from, method,
                per_km_rate::float8 AS per_km_rate, vat_rate::float8 AS vat_rate,
                rental_mode, rental_amount::float8 AS rental_amount
           FROM pricing_rules
          WHERE company = $1 AND effective_from <= (now() AT TIME ZONE 'Africa/Cairo')::date
          ORDER BY effective_from DESC LIMIT 1",
    )
    .bind(&company)
    .fetch_optional(ctx.pool)
    .await
    .map_err(|e| e.to_string())?
    .ok_or(format!("no pricing rule in force today for `{company}`; nothing to price its routes with"))?;

    // Every route of the company: what it is and where its terminal is. Its
    // price comes from FalconGo's engine below — never re-derived here.
    #[derive(sqlx::FromRow)]
    struct Route {
        terminal: String,
        drop_off_point: String,
        distance: f64,
        band: Option<i32>,
        t_lat: Option<f64>,
        t_lng: Option<f64>,
    }
    let routes: Vec<Route> = sqlx::query_as(
        "SELECT fm.terminal, fm.drop_off_point, COALESCE(fm.distance, 0)::float8 AS distance,
                CASE WHEN $3::text = 'band_per_1000l' THEN fm.fee::int END AS band,
                NULLIF(t.latitude, 0)::float8 AS t_lat, NULLIF(t.longitude, 0)::float8 AS t_lng
           FROM fee_mappings fm
           LEFT JOIN terminals t ON t.id = fm.terminal_id AND t.deleted_at IS NULL
          WHERE fm.deleted_at IS NULL AND fm.company = $1
            AND ($2::text IS NULL OR fm.terminal = $2)",
    )
    .bind(&company)
    .bind(s(args, "terminal"))
    .bind(&rule.method)
    .fetch_all(ctx.pool)
    .await
    .map_err(|e| e.to_string())?;
    if routes.is_empty() {
        return Err(format!("no fee mappings (routes) for company `{company}`"));
    }

    // What each route earns, from FalconGo's pricing engine — the same one
    // that prices and stores every trip. A per-km rule ignores the tank, so it
    // is quoted even when the tank is unknown.
    let quote_tank = match (tank, rule.method.as_str()) {
        (Some(t), _) => Some(t),
        (None, "per_km") => Some(1),
        (None, _) => None,
    };
    let mut quotes: std::collections::HashMap<(String, String), (Option<f64>, Option<f64>, Option<String>)> =
        Default::default();
    if let Some(t) = quote_tank {
        let mut query = vec![("company", company.clone()), ("tank_capacity", t.to_string())];
        if let Some(term) = s(args, "terminal") {
            query.push(("terminal", term.to_string()));
        }
        let body = upstream::falcon_get(&ctx.adv.http, &ctx.adv.cfg, ctx.token, "/pricing/quote", &query)
            .await
            .map_err(|e| format!("could not price the routes: {e}"))?;
        for q in body["quotes"].as_array().into_iter().flatten() {
            quotes.insert(
                (
                    q["terminal"].as_str().unwrap_or_default().to_string(),
                    q["drop_off_point"].as_str().unwrap_or_default().to_string(),
                ),
                (
                    q["base_revenue"].as_f64(),
                    q["revenue_vat"].as_f64(),
                    q["pricing_error"].as_str().map(str::to_string),
                ),
            );
        }
    }

    // Where it is now, from its tracker (cars.latitude/longitude are stale).
    let live = fleet::position(ctx, car_id).await;
    let here = live.map(|(p, _)| p);

    // Empty km to each terminal, once per terminal; road distance for the
    // nearest few when OSRM is configured, straight-line otherwise.
    let mut terminals: Vec<(String, Option<(f64, f64)>)> = Vec::new();
    for r in &routes {
        if !terminals.iter().any(|(n, _)| n == &r.terminal) {
            terminals.push((r.terminal.clone(), r.t_lat.zip(r.t_lng)));
        }
    }
    let mut empty_km: std::collections::HashMap<String, (f64, &'static str)> = Default::default();
    if let Some(here) = here {
        let mut by_air: Vec<(String, (f64, f64), f64)> = terminals
            .iter()
            .filter_map(|(n, p)| p.map(|p| (n.clone(), p, haversine_km(here, p))))
            .collect();
        by_air.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
        for (idx, (name, pos, air)) in by_air.into_iter().enumerate() {
            let road = if idx < 6 {
                upstream::osrm_route(&ctx.adv.http, &ctx.adv.cfg, here, pos).await
            } else {
                None
            };
            empty_km.insert(
                name,
                match road {
                    Some((km, _)) => (km, "road"),
                    None => (air * 1.3, "straight_line_x1.3"),
                },
            );
        }
    }

    let round2 = |v: f64| (v * 100.0).round() / 100.0;
    let mut unpriced = Vec::new();
    let mut options: Vec<Value> = Vec::new();
    for r in &routes {
        let quote = quotes.get(&(r.terminal.clone(), r.drop_off_point.clone()));
        let (Some(base), Some(vat)) = (
            quote.and_then(|q| q.0).filter(|b| *b > 0.0),
            quote.and_then(|q| q.1),
        ) else {
            let why = match (quote_tank, quote) {
                (None, _) => "the truck's tank capacity is unknown".to_string(),
                (_, Some((_, _, Some(err)))) => err.clone(),
                (_, None) => "FalconGo returned no quote for this route".to_string(),
                _ => "the route prices to nothing".to_string(),
            };
            unpriced.push(json!({ "terminal": r.terminal, "drop_off_point": r.drop_off_point, "why": why }));
            continue;
        };
        let empty = empty_km.get(&r.terminal).copied();
        // A delivery is terminal → drop-off and back, plus the empty run to
        // reach the terminal.
        let total_km = 2.0 * r.distance + empty.map(|e| e.0).unwrap_or(0.0);
        options.push(json!({
            "terminal": r.terminal,
            "drop_off_point": r.drop_off_point,
            "band": r.band,
            "distance_km": r.distance,
            "expected_base_revenue": round2(base),
            "expected_vat": round2(vat),
            "expected_total": round2(base + vat),
            "base_per_km_round_trip": (r.distance > 0.0).then(|| round2(base / (2.0 * r.distance))),
            "empty_km_to_terminal": empty.map(|e| (e.0 * 10.0).round() / 10.0),
            "empty_km_method": empty.map(|e| e.1),
            "total_km": (total_km * 10.0).round() / 10.0,
            "base_per_total_km": (total_km > 0.0).then(|| round2(base / total_km)),
        }));
    }
    options.sort_by(|a, b| {
        let f = |v: &Value| v["base_per_total_km"].as_f64().unwrap_or(0.0);
        f(b).partial_cmp(&f(a)).unwrap_or(std::cmp::Ordering::Equal)
    });
    options.truncate(40);
    Ok(json!({
        "car_id": car_id, "plate": plate, "company": company,
        "tank_capacity_l": tank, "tank_capacity_from": tank_source,
        "pricing_rule": {
            "id": rule.id, "effective_from": rule.effective_from, "method": rule.method,
            "per_km_rate": rule.per_km_rate, "vat_rate": rule.vat_rate,
            "rental_mode": rule.rental_mode, "rental_amount": rule.rental_amount,
        },
        "rental_note": "Car rental (TAQA monthly, Petromin per car-day) is earned per truck-month whichever route it runs, so it is not in these per-delivery figures; add it when comparing companies.",
        "truck_position_known": here.is_some(),
        "position_fix_cairo": live.and_then(|(_, t)| t).as_ref().map(fleet::cairo),
        "options": options,
        "unpriced_routes": unpriced,
    }))
}

async fn search_history(ctx: &ToolCtx<'_>, args: &Value) -> ToolResult {
    let variants: Vec<String> = args
        .get("variants")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    if variants.is_empty() {
        return Err("`variants` needs at least one search phrase".into());
    }
    let filters = search::Filters {
        car_id: i(args, "car_id"),
        plate: None,
        since: s(args, "since").and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()),
    };
    let limit = i(args, "limit").unwrap_or(25).clamp(1, 50) as usize;
    let hits = search::hybrid(ctx.pool, &ctx.adv.http, &ctx.adv.cfg, &variants, &filters, limit).await?;
    Ok(json!({ "count": hits.len(), "hits": hits }))
}

async fn maintenance_status(ctx: &ToolCtx<'_>, args: &Value) -> ToolResult {
    let a = &ctx.adv;
    let mut q = Vec::new();
    if let Some(id) = i(args, "car_id") {
        q.push(("vehicle_id", id.to_string()));
    }
    let due = upstream::maint_get(&a.http, &a.cfg, ctx.token, "/due", &q)
        .await
        .map_err(|e| e.to_string())?;
    let due: Vec<Value> = due
        .as_array()
        .map(|a| a.iter().filter(|r| r["status"] != "ok").take(150).cloned().collect())
        .unwrap_or_default();
    let mut wq = q.clone();
    wq.push(("status", "open".into()));
    let open = upstream::maint_get(&a.http, &a.cfg, ctx.token, "/work-orders", &wq)
        .await
        .unwrap_or_else(|e| json!({ "error": e.to_string() }));
    Ok(json!({ "due_or_overdue": due, "open_work_orders": open }))
}

async fn propose_change(ctx: &ToolCtx<'_>, args: &Value) -> ToolResult {
    let kind = s(args, "kind").ok_or("`kind` is required")?;
    let params = args.get("params").cloned().unwrap_or_else(|| json!({}));
    let title = s(args, "title").ok_or("`title` is required")?;
    let rationale = s(args, "rationale").ok_or("`rationale` is required")?;
    let mut planned = actions::plan(kind, &params)?;

    // The record as it is now: for the reviewer, for the version guard, and
    // for full-row edits, the fields the proposal leaves alone.
    let before = read_record(ctx, &mut planned).await?;
    if planned.meta.get("merge").is_some() {
        falcon_actions::merge_into(&mut planned.body, &planned.meta, &before);
    }
    enrich(ctx, &mut planned).await?;
    check_references(ctx.pool, &planned.body).await?;

    let (before_view, after_view) = review_views(&planned, &before);
    let id = store::insert_suggestion(
        ctx.pool,
        store::NewSuggestion {
            conversation_id: ctx.conversation_id,
            user_id: ctx.user_id,
            title,
            rationale,
            before: before_view,
            after: after_view,
            planned: &planned,
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    if let Ok(mut p) = ctx.proposed.lock() {
        p.push(id);
    }
    Ok(json!({
        "suggestion_id": id,
        "status": "pending",
        "service": planned.target.as_str(),
        "note": "Shown to the user with Approve / Reject. Do not claim it is done."
    }))
}

/// Keys never shown to the model or the reviewer.
fn is_secret(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k == "pin" || k.contains("password") || k.contains("token") || k.contains("secret") || k == "embedding"
}

fn scrub(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.into_iter().filter(|(k, _)| !is_secret(k)).collect()),
        other => other,
    }
}

async fn read_record(ctx: &ToolCtx<'_>, planned: &mut actions::Planned) -> Result<Value, String> {
    match planned.record.clone() {
        actions::Record::None => Ok(json!({})),
        actions::Record::Falcon { table, id } => {
            // `table` comes from the static catalog, never from the model.
            let row: Option<Value> = sqlx::query_scalar(&format!(
                "SELECT to_jsonb(t) FROM {table} t WHERE id = $1 AND deleted_at IS NULL"
            ))
            .bind(id)
            .fetch_optional(ctx.pool)
            .await
            .map_err(|e| e.to_string())?;
            let mut row = scrub(row.ok_or(format!("no {table} row with id {id}"))?);
            if table == "service_invoices" {
                let items: Value = sqlx::query_scalar(
                    "SELECT COALESCE(jsonb_agg(jsonb_build_object('service', service, 'notes', notes)
                                               ORDER BY item_order, id), '[]'::jsonb)
                       FROM inspection_items WHERE service_invoice_id = $1 AND deleted_at IS NULL",
                )
                .bind(id)
                .fetch_one(ctx.pool)
                .await
                .map_err(|e| e.to_string())?;
                row["inspection_items"] = items;
            }
            Ok(row)
        }
        actions::Record::ApexTransaction { id } => {
            let row: Option<Value> = sqlx::query_scalar(
                "SELECT to_jsonb(t) FROM banksms.transactions t WHERE id = $1 AND deleted_at IS NULL",
            )
            .bind(id)
            .fetch_optional(ctx.pool)
            .await
            .map_err(|e| e.to_string())?;
            let row = row.ok_or(format!("no ledger transaction with id {id}"))?;
            // A split part's parent carries the version the split routes check.
            let version = match (planned.kind, row.get("parent_id").and_then(Value::as_i64)) {
                ("split_transaction", Some(_)) => {
                    return Err("that transaction is a split part; split its parent instead".into())
                }
                _ => row.get("version").cloned().unwrap_or(Value::Null),
            };
            planned.meta["if_match"] = version;
            Ok(scrub(row))
        }
        actions::Record::Maint { entity_type, entity_id } => {
            let found = upstream::maint_record(&ctx.adv.http, &ctx.adv.cfg, ctx.token, &entity_type, &entity_id)
                .await
                .map_err(|e| e.to_string())?;
            let ops_need_row = planned.body["operations"]
                .as_array()
                .is_some_and(|ops| ops.iter().any(|o| o["entity_id"] == entity_id.as_str() && o["operation"] != "insert"));
            match found {
                Some((row, version)) => {
                    actions::maint::set_version(planned, &entity_id, version);
                    Ok(scrub(row))
                }
                None if ops_need_row => Err(format!("no maint-rust {entity_type} with id {entity_id}")),
                None => Ok(json!({})),
            }
        }
    }
}

/// Fill what the database knows better than the model: plates and driver
/// names for trips, an invoice's plate, the readings an odometer correction
/// outranks; and check a company name before moving a truck to it.
async fn enrich(ctx: &ToolCtx<'_>, planned: &mut actions::Planned) -> Result<(), String> {
    let pool = ctx.pool;
    let plate = |car: i64| async move {
        sqlx::query_scalar::<_, String>("SELECT car_no_plate FROM cars WHERE id = $1 AND deleted_at IS NULL")
            .bind(car)
            .fetch_optional(pool)
            .await
            .map_err(|e| e.to_string())?
            .ok_or(format!("no truck with car id {car}"))
    };
    let driver = |id: i64| async move {
        sqlx::query_scalar::<_, String>("SELECT name FROM drivers WHERE id = $1 AND deleted_at IS NULL")
            .bind(id)
            .fetch_optional(pool)
            .await
            .map_err(|e| e.to_string())?
            .ok_or(format!("no driver with id {id}"))
    };

    if planned.meta.get("enrich").and_then(Value::as_str) == Some("trip") {
        let parent = &mut planned.body["parent_trip"];
        if let Some(car) = parent.get("car_id").and_then(Value::as_i64) {
            parent["car_no_plate"] = json!(plate(car).await?);
        }
        if let Some(d) = parent.get("driver_id").and_then(Value::as_i64) {
            parent["driver_name"] = json!(driver(d).await?);
        }
    }
    match planned.kind {
        "update_trip" => {
            if let Some(car) = planned.body.get("car_id").and_then(Value::as_i64) {
                if planned.body.get("car_no_plate").is_none() {
                    planned.body["car_no_plate"] = json!(plate(car).await?);
                }
            }
            if let Some(d) = planned.body.get("driver_id").and_then(Value::as_i64) {
                if planned.body.get("driver_name").is_none() {
                    planned.body["driver_name"] = json!(driver(d).await?);
                }
            }
        }
        "create_service_invoice" => {
            if planned.body.get("plate_number").is_none() {
                let car = planned.body["car_id"].as_i64().unwrap_or(0);
                planned.body["plate_number"] = json!(plate(car).await?);
            }
        }
        "reassign_car" => {
            if let Some(c) = planned.body.get("operating_company").and_then(Value::as_str) {
                let known: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM fee_mappings WHERE company = $1 AND deleted_at IS NULL)
                         OR EXISTS (SELECT 1 FROM cars WHERE operating_company = $1 AND deleted_at IS NULL)",
                )
                .bind(c)
                .fetch_one(pool)
                .await
                .map_err(|e| e.to_string())?;
                if !known {
                    return Err(format!("`{c}` is not a company in fee_mappings or on any truck; check the spelling"));
                }
            }
        }
        "set_odometer" | "log_oil_change" | "record_service" => {
            let car = planned.body["operations"][0]["payload"]["vehicle_id"]
                .as_i64()
                .or_else(|| planned.body["operations"][0]["entity_id"].as_str().and_then(|s| s.parse().ok()))
                .unwrap_or(0);
            plate(car).await?;
            if planned.kind == "set_odometer" {
                // The automatic readings this correction outranks: Falcon's
                // latest fuel odometer and its mileage (the max of fuel, oil
                // and service readings), as maint-rust's view compares them.
                let seen: i64 = sqlx::query_scalar(
                    "SELECT GREATEST(COALESCE(last_fuel_odometer, 0)::bigint,
                                     COALESCE(mileage, 0)::bigint)
                       FROM cars WHERE id = $1",
                )
                .bind(car)
                .fetch_one(pool)
                .await
                .map_err(|e| e.to_string())?;
                planned.body["operations"][0]["payload"]["superseded_km"] = json!(seen);
            }
        }
        _ => {}
    }
    Ok(())
}

/// Every car_id / driver_id a body names must exist: several FalconGo
/// handlers would otherwise write a row pointing at nothing.
async fn check_references(pool: &PgPool, body: &Value) -> Result<(), String> {
    fn walk(v: &Value, cars: &mut Vec<i64>, drivers: &mut Vec<i64>) {
        match v {
            Value::Object(m) => {
                for (k, v) in m {
                    match (k.as_str(), v.as_i64()) {
                        ("car_id", Some(id)) => cars.push(id),
                        ("driver_id", Some(id)) => drivers.push(id),
                        _ => walk(v, cars, drivers),
                    }
                }
            }
            Value::Array(a) => a.iter().for_each(|v| walk(v, cars, drivers)),
            _ => {}
        }
    }
    let (mut cars, mut drivers) = (Vec::new(), Vec::new());
    walk(body, &mut cars, &mut drivers);
    for (table, ids) in [("cars", cars), ("drivers", drivers)] {
        for id in ids {
            let ok: bool = sqlx::query_scalar(&format!(
                "SELECT EXISTS (SELECT 1 FROM {table} WHERE id = $1 AND deleted_at IS NULL)"
            ))
            .bind(id)
            .fetch_one(pool)
            .await
            .map_err(|e| e.to_string())?;
            if !ok {
                return Err(format!("no {} with id {id}", if table == "cars" { "truck" } else { "driver" }));
            }
        }
    }
    Ok(())
}

/// What the reviewer sees: for an edit, only the fields that change (and the
/// record's name), before and after; for a create, what will be written; for
/// a delete, the record that goes.
fn review_views(planned: &actions::Planned, before: &Value) -> (Value, Value) {
    const LABELS: &[&str] = &["car_no_plate", "plate_number", "name", "driver_name", "date", "company", "receipt_no"];
    let mut changed = serde_json::Map::new();
    let flatten = |v: &Value, out: &mut serde_json::Map<String, Value>| {
        if let Value::Object(m) = v {
            for (k, v) in m {
                match v {
                    Value::Object(inner) if k != "payload" => {
                        for (ik, iv) in inner {
                            out.insert(ik.clone(), iv.clone());
                        }
                    }
                    _ => {
                        out.insert(k.clone(), v.clone());
                    }
                }
            }
        }
    };
    // A maint batch is reviewed by its first operation's payload, plus the
    // categories of a service record's tasks.
    match planned.body["operations"].as_array() {
        Some(ops) if !ops.is_empty() => {
            flatten(&ops[0]["payload"], &mut changed);
            let tasks: Vec<&str> = ops[1..].iter().filter_map(|o| o["payload"]["category_id"].as_str()).collect();
            if !tasks.is_empty() {
                changed.insert("tasks".into(), json!(tasks.join(", ")));
            }
        }
        _ => flatten(&planned.body, &mut changed),
    }
    // Lists (a trip's containers, an invoice's lines) read as one line each.
    for v in changed.values_mut() {
        if let Value::Array(items) = v {
            let lines: Vec<String> = items
                .iter()
                .map(|it| match it {
                    Value::Object(m) => m
                        .values()
                        .filter(|x| !x.is_null() && x.as_str() != Some(""))
                        .map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string()))
                        .collect::<Vec<_>>()
                        .join(" · "),
                    other => other.as_str().map(str::to_string).unwrap_or_else(|| other.to_string()),
                })
                .collect();
            *v = json!(lines.join(" | "));
        }
    }
    let is_delete = planned.method == "DELETE" || planned.kind.starts_with("delete_");
    let empty = before.as_object().map_or(true, |m| m.is_empty());
    if is_delete {
        // The record that goes, without bookkeeping columns or empty values.
        let gone: serde_json::Map<String, Value> = before
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(k, v)| {
                !matches!(k.as_str(), "id" | "ID" | "created_at" | "updated_at" | "deleted_at" | "version" | "sync_version")
                    && !k.ends_with("_image_name")
                    && !k.ends_with("_image_name_back")
                    && !v.is_null()
                    && v.as_str() != Some("")
                    && !v.is_object()
                    && !v.is_array()
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        return (Value::Object(gone), json!({}));
    }
    if empty {
        changed.retain(|k, _| k != "ID" && k != "ref_id");
        return (json!({}), Value::Object(changed));
    }
    let mut b = serde_json::Map::new();
    let mut a = serde_json::Map::new();
    for label in LABELS {
        if let Some(v) = before.get(*label).filter(|v| !v.is_null() && v.as_str() != Some("")) {
            b.insert((*label).into(), v.clone());
            a.insert((*label).into(), v.clone());
        }
    }
    for (k, v) in changed {
        if k == "ID" || k == "id" || before.get(&k) == Some(&v) {
            continue;
        }
        b.insert(k.clone(), before.get(&k).cloned().unwrap_or(Value::Null));
        a.insert(k, v);
    }
    (Value::Object(b), Value::Object(a))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_an_object_schema_and_a_unique_name() {
        let defs = tool_defs();
        let mut names: Vec<_> = defs.iter().map(|d| d.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), defs.len());
        for d in &defs {
            assert_eq!(d.parameters["type"], "object", "{}", d.name);
        }
    }

    #[test]
    fn a_bare_day_asks_etit_for_that_date() {
        assert_eq!(range_query(&json!({ "from": "2026-10-20" })).unwrap(), vec![("date", "2026-10-20".to_string())]);
        let r = range_query(&json!({ "from": "2026-10-20", "to": "2026-10-22" })).unwrap();
        assert_eq!(r[0].1, "2026-10-20T00:00:00");
        assert_eq!(r[1].1, "2026-10-22T23:59:59");
    }

    #[test]
    fn haversine_is_in_kilometres() {
        // Cairo → Alexandria is ~180 km as the crow flies.
        let d = haversine_km((30.0444, 31.2357), (31.2001, 29.9187));
        assert!((170.0..195.0).contains(&d), "{d}");
    }

    #[test]
    fn history_is_condensed_to_places() {
        let h = json!({ "points": [1, 2, 3], "stops": [{ "lat": 1 }], "pins": [], "legs": [] });
        let c = condense_history(&h);
        assert!(c.get("points").is_none());
        assert_eq!(c["stop_count"], 1);
    }
}
