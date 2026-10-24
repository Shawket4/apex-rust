//! The advisor's tools: what Claude can look at, and the one thing it can do
//! (propose a change for an admin to approve).
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
use super::{actions, search, sql, store, Advisor};

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
            name: "fleet_overview",
            description: "Every active truck with its operating company and area, driver, whether it is in a trip, last known location time, and its trips/revenue over the last N days; plus per-company totals. The starting point for utilisation and rebalancing questions.".into(),
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
            description: "Revenue (trips.revenue, the fee for each delivery), trip count and mileage between two dates, grouped by car, company, terminal, drop_off_point, driver or day. Grouped by car it also subtracts fuel, oil-change and service costs for a per-truck margin (driver pay excluded).".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "from": date, "to": date,
                    "group_by": { "type": "string", "enum": ["car", "company", "terminal", "drop_off_point", "driver", "day"] },
                    "company": { "type": "string" },
                    "car_id": { "type": "integer" }
                },
                "required": ["from", "to", "group_by"]
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
            description: "Where a truck could earn the most: every (terminal, drop-off) fee mapping for a company with fee, distance and fee per km, plus the empty km from the truck's current position to each terminal (road distance when available). Rank by fee per total km to suggest better work.".into(),
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
            name: "propose_change",
            description: format!(
                "Propose ONE change for an admin to approve. Nothing changes until they approve it; then it is applied through FalconGo's API in their name, marked as coming from this chat. Only propose what the data supports, and say why. Kinds: {}",
                actions::kinds_schema()
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ["reassign_car", "assign_driver"] },
                    "params": { "type": "object" },
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
        "revenue_breakdown" => revenue_breakdown(ctx, args).await,
        "truck_itinerary" => truck_itinerary(ctx, args).await,
        "trip_audit" => trip_audit(ctx, args).await,
        "route_options" => route_options(ctx, args).await,
        "search_service_history" => search_history(ctx, args).await,
        "maintenance_status" => maintenance_status(ctx, args).await,
        "run_sql" => {
            let q = args.get("query").and_then(Value::as_str).unwrap_or("");
            Ok(sql::run(ctx.pool, q).await)
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
                c.is_in_trip, c.driver_id, d.name AS driver_name, c.etit_car_id,
                c.slack_status, c.geo_fence
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

async fn fleet_overview(ctx: &ToolCtx<'_>, args: &Value) -> ToolResult {
    let days = i(args, "days").unwrap_or(30).clamp(1, 120);
    let company = s(args, "company").map(str::to_string);
    let trucks = rows(
        ctx.pool,
        "WITH t AS (
            SELECT car_id, count(*) AS trips, COALESCE(sum(revenue), 0) AS revenue,
                   max(date) AS last_trip_date
              FROM trips
             WHERE deleted_at IS NULL
               AND date >= to_char(current_date - $1::int, 'YYYY-MM-DD')
             GROUP BY car_id)
         SELECT c.id AS car_id, c.car_no_plate, c.operating_company, c.operating_area,
                c.is_in_trip, d.name AS driver_name, c.location_time_stamp,
                c.slack_status, c.geo_fence,
                COALESCE(t.trips, 0) AS trips, round(COALESCE(t.revenue, 0)::numeric, 0) AS revenue,
                t.last_trip_date
           FROM cars c
           LEFT JOIN drivers d ON d.id = c.driver_id
           LEFT JOIN t ON t.car_id = c.id
          WHERE c.deleted_at IS NULL
            AND ($2::text IS NULL OR c.operating_company = $2)
          ORDER BY c.operating_company, revenue DESC",
        vec![json!(days), json!(company)],
    )
    .await?;
    let companies = rows(
        ctx.pool,
        "WITH t AS (
            SELECT car_id, count(*) AS trips, COALESCE(sum(revenue), 0) AS revenue
              FROM trips
             WHERE deleted_at IS NULL
               AND date >= to_char(current_date - $1::int, 'YYYY-MM-DD')
             GROUP BY car_id)
         SELECT COALESCE(NULLIF(c.operating_company, ''), '(none)') AS operating_company,
                count(*) AS trucks,
                count(*) FILTER (WHERE COALESCE(t.trips, 0) = 0) AS idle_trucks,
                COALESCE(sum(t.trips), 0) AS trips,
                round(COALESCE(sum(t.revenue), 0)::numeric, 0) AS revenue,
                round((COALESCE(sum(t.revenue), 0) / NULLIF(count(*), 0))::numeric, 0) AS revenue_per_truck
           FROM cars c LEFT JOIN t ON t.car_id = c.id
          WHERE c.deleted_at IS NULL
            AND ($2::text IS NULL OR c.operating_company = $2)
          GROUP BY 1 ORDER BY revenue DESC",
        vec![json!(days), json!(s(args, "company"))],
    )
    .await?;
    Ok(json!({ "window_days": days, "companies": companies, "trucks": trucks }))
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
    let base = format!(
        "SELECT {key} AS key, min(t.car_no_plate) AS plate_example,
                count(*) AS trips, round(sum(t.revenue)::numeric, 0) AS revenue,
                round(sum(t.mileage)::numeric, 0) AS mileage_km,
                round((sum(t.revenue) / NULLIF(count(*), 0))::numeric, 0) AS revenue_per_trip
           FROM trips t
          WHERE t.deleted_at IS NULL AND t.date BETWEEN $1 AND $2
            AND ($3::text IS NULL OR t.company = $3)
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
                    round((r.revenue - COALESCE(f.fuel, 0) - COALESCE(o.oil, 0) - COALESCE(sv.service, 0))::numeric, 0) AS margin_before_driver
               FROM r
               LEFT JOIN cars c ON c.id::text = r.key
               LEFT JOIN f ON f.key = r.key
               LEFT JOIN o ON o.key = r.key
               LEFT JOIN sv ON sv.key = r.key
              ORDER BY margin_before_driver DESC"
        )
    } else {
        format!("{base} ORDER BY revenue DESC")
    };
    let out = rows(
        ctx.pool,
        &query,
        vec![json!(from), json!(to), json!(s(args, "company")), json!(i(args, "car_id"))],
    )
    .await?;
    Ok(json!({ "from": from, "to": to, "group_by": group, "rows": out }))
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
    mq.push(("sort", "excess".into()));
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
    type CarPosition = (String, Option<String>, Option<String>, Option<String>);
    let car: Option<CarPosition> = sqlx::query_as(
        "SELECT car_no_plate, operating_company, latitude, longitude FROM cars WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(car_id)
    .fetch_optional(ctx.pool)
    .await
    .map_err(|e| e.to_string())?;
    let (plate, op_company, lat, lng) = car.ok_or(format!("no car with id {car_id}"))?;
    let company = s(args, "company")
        .map(str::to_string)
        .or(op_company.filter(|c| !c.trim().is_empty()))
        .ok_or("the truck has no operating company; pass `company`")?;
    let here = match (lat.and_then(|v| v.parse::<f64>().ok()), lng.and_then(|v| v.parse::<f64>().ok())) {
        (Some(a), Some(b)) if a != 0.0 && b != 0.0 => Some((a, b)),
        _ => None,
    };

    #[derive(sqlx::FromRow)]
    struct Mapping {
        terminal: String,
        drop_off_point: String,
        distance: f64,
        fee: f64,
        t_lat: Option<f64>,
        t_lng: Option<f64>,
    }
    let mappings: Vec<Mapping> = sqlx::query_as(
        "SELECT fm.terminal, fm.drop_off_point, fm.distance::float8 AS distance, fm.fee::float8 AS fee,
                NULLIF(t.latitude, 0)::float8 AS t_lat, NULLIF(t.longitude, 0)::float8 AS t_lng
           FROM fee_mappings fm
           LEFT JOIN terminals t ON t.id = fm.terminal_id AND t.deleted_at IS NULL
          WHERE fm.deleted_at IS NULL AND fm.company = $1
            AND ($2::text IS NULL OR fm.terminal = $2)
            AND fm.fee > 0",
    )
    .bind(&company)
    .bind(s(args, "terminal"))
    .fetch_all(ctx.pool)
    .await
    .map_err(|e| e.to_string())?;
    if mappings.is_empty() {
        return Err(format!("no fee mappings for company `{company}`"));
    }

    // Empty km to each terminal, once per terminal; road distance for the
    // nearest few when OSRM is configured, straight-line otherwise.
    let mut terminals: Vec<(String, Option<(f64, f64)>)> = Vec::new();
    for m in &mappings {
        if !terminals.iter().any(|(n, _)| n == &m.terminal) {
            terminals.push((m.terminal.clone(), m.t_lat.zip(m.t_lng)));
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

    let mut options: Vec<Value> = mappings
        .iter()
        .map(|m| {
            let empty = empty_km.get(&m.terminal).copied();
            // A delivery is terminal → drop-off and back: 2 × distance loaded
            // and return, plus the empty run to reach the terminal.
            let total = 2.0 * m.distance + empty.map(|e| e.0).unwrap_or(0.0);
            json!({
                "terminal": m.terminal,
                "drop_off_point": m.drop_off_point,
                "fee": m.fee,
                "distance_km": m.distance,
                "fee_per_km_round_trip": if m.distance > 0.0 { Some((m.fee / (2.0 * m.distance) * 100.0).round() / 100.0) } else { None },
                "empty_km_to_terminal": empty.map(|e| (e.0 * 10.0).round() / 10.0),
                "empty_km_method": empty.map(|e| e.1),
                "fee_per_total_km": if total > 0.0 { Some((m.fee / total * 100.0).round() / 100.0) } else { None },
            })
        })
        .collect();
    options.sort_by(|a, b| {
        let f = |v: &Value| v["fee_per_total_km"].as_f64().unwrap_or(0.0);
        f(b).partial_cmp(&f(a)).unwrap_or(std::cmp::Ordering::Equal)
    });
    options.truncate(40);
    Ok(json!({
        "car_id": car_id, "plate": plate, "company": company,
        "truck_position_known": here.is_some(),
        "options": options,
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
    let planned = actions::plan(kind, &params)?;

    // Check the targets exist and capture what the reviewer will see change.
    let car_id = i(&params, "car_id").unwrap_or(0);
    type CarAssignment = (String, Option<String>, Option<String>, Option<i64>);
    let car: Option<CarAssignment> = sqlx::query_as(
        "SELECT car_no_plate, operating_company, operating_area, driver_id::bigint
           FROM cars WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(car_id)
    .fetch_optional(ctx.pool)
    .await
    .map_err(|e| e.to_string())?;
    let (plate, company, area, driver_id) = car.ok_or(format!("no car with id {car_id}"))?;
    let (before, after) = match planned.kind {
        "reassign_car" => {
            let new_company = planned.body.get("operating_company").cloned();
            if let Some(c) = new_company.as_ref().and_then(Value::as_str) {
                let known: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM fee_mappings WHERE company = $1 AND deleted_at IS NULL)
                         OR EXISTS (SELECT 1 FROM cars WHERE operating_company = $1 AND deleted_at IS NULL)",
                )
                .bind(c)
                .fetch_one(ctx.pool)
                .await
                .map_err(|e| e.to_string())?;
                if !known {
                    return Err(format!("`{c}` is not a company in fee_mappings or on any truck; check the spelling"));
                }
            }
            (
                json!({ "plate": plate, "operating_company": company, "operating_area": area }),
                json!({
                    "plate": plate,
                    "operating_company": new_company.unwrap_or(json!(company)),
                    "operating_area": planned.body.get("operating_area").cloned().unwrap_or(json!(area)),
                }),
            )
        }
        "assign_driver" => {
            let new_driver = i(&params, "driver_id").unwrap_or(0);
            let name: Option<String> = sqlx::query_scalar(
                "SELECT name FROM drivers WHERE id = $1 AND deleted_at IS NULL",
            )
            .bind(new_driver)
            .fetch_optional(ctx.pool)
            .await
            .map_err(|e| e.to_string())?;
            let name = name.ok_or(format!("no driver with id {new_driver}"))?;
            let old: Option<String> = match driver_id {
                Some(d) => sqlx::query_scalar("SELECT name FROM drivers WHERE id = $1")
                    .bind(d)
                    .fetch_optional(ctx.pool)
                    .await
                    .map_err(|e| e.to_string())?,
                None => None,
            };
            (
                json!({ "plate": plate, "driver_id": driver_id, "driver": old }),
                json!({ "plate": plate, "driver_id": new_driver, "driver": name }),
            )
        }
        _ => (json!({}), json!({})),
    };

    let id = store::insert_suggestion(
        ctx.pool,
        store::NewSuggestion {
            conversation_id: ctx.conversation_id,
            user_id: ctx.user_id,
            title,
            rationale,
            before,
            after,
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
        "note": "Shown to the user with Approve / Reject. Do not claim it is done."
    }))
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
