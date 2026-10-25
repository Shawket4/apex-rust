//! Where the trucks are now, from the systems that know.
//!
//! FalconGo's `cars.latitude/longitude/location_time_stamp/engine_status/
//! speed/slack_status/geo_fence/is_in_trip` have had no automatic writer since
//! the ETIT scraper was removed; they are a year stale and must never answer
//! "where is it / what is it doing". The truth is:
//!
//! * live position, speed and status — etit-proxy's `/vehicles/live` (polled
//!   from ETIT every minute), joined to FalconGo cars on `cars.etit_car_id`;
//! * which place a truck is at — FalconGo's `terminals` and `drop_off_points`
//!   pins and radii (the same circles the trip audit uses: 500 m / 300 m by
//!   default);
//! * dead zones — etit-proxy's restricted areas (`/zones`): circles trucks
//!   must not enter. Past violations come from a manual zone scan
//!   (`POST /zones/scan {from, to}`), which reports without alerting; an
//!   empty-body scan is the scheduled round that WhatsApps the group, so it is
//!   never sent from here;
//! * who is driving — `driver_sessions` opened by the driver tablet.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use chrono_tz::Africa::Cairo;
use serde_json::{json, Value};

use super::tools::ToolCtx;
use super::upstream;

/// A fix older than this is "no signal", whatever the status says.
pub const NO_SIGNAL_AFTER_MIN: i64 = 30;
const TERMINAL_RADIUS_M: f64 = 500.0;
const DROPOFF_RADIUS_M: f64 = 300.0;

/// ETIT's numeric status, as etit-proxy and the live map read it.
pub fn status_name(code: i64) -> &'static str {
    match code {
        1 | 4 => "moving",
        3 => "stopped",
        12 => "offline",
        13 => "idling",
        17 | 18 => "geofence_violation",
        _ => "unknown",
    }
}

pub fn haversine_m(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (lat1, lon1) = (a.0.to_radians(), a.1.to_radians());
    let (lat2, lon2) = (b.0.to_radians(), b.1.to_radians());
    let h = ((lat2 - lat1) / 2.0).sin().powi(2)
        + lat1.cos() * lat2.cos() * ((lon2 - lon1) / 2.0).sin().powi(2);
    2.0 * 6_371_000.0 * h.sqrt().asin()
}

pub fn cairo(ts: &DateTime<Utc>) -> String {
    ts.with_timezone(&Cairo).format("%Y-%m-%d %H:%M").to_string()
}

#[derive(Debug, Clone)]
pub struct Place {
    pub kind: &'static str,
    pub name: String,
    pub at: (f64, f64),
    pub radius_m: f64,
}

#[derive(Debug, Clone)]
pub struct Zone {
    pub id: i64,
    pub name: String,
    pub at: (f64, f64),
    pub radius_m: f64,
}

/// One live fix, as `/vehicles/live` gives it.
#[derive(Debug, Clone)]
pub struct Fix {
    pub at: (f64, f64),
    pub speed: f64,
    pub status: i64,
    pub ts: Option<DateTime<Utc>>,
    pub event: Option<String>,
}

fn f64_of(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// The live snapshot keyed by ETIT vehicle id. Positions of (0, 0) are ETIT's
/// "no coordinates" and are dropped.
pub fn parse_live(v: &Value) -> HashMap<String, Fix> {
    let mut out = HashMap::new();
    for row in v.as_array().into_iter().flatten() {
        let Some(id) = row.get("id").and_then(Value::as_str) else { continue };
        let (Some(lat), Some(lng)) = (row.get("lat").and_then(f64_of), row.get("lng").and_then(f64_of)) else {
            continue;
        };
        if lat == 0.0 && lng == 0.0 {
            continue;
        }
        out.insert(
            id.to_string(),
            Fix {
                at: (lat, lng),
                speed: row.get("speed").and_then(f64_of).unwrap_or(0.0),
                status: row.get("status").and_then(|s| s.as_i64().or_else(|| s.as_str().and_then(|x| x.parse().ok()))).unwrap_or(-1),
                ts: row
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                    .map(|t| t.with_timezone(&Utc)),
                event: row.get("event").and_then(Value::as_str).map(str::to_string),
            },
        );
    }
    out
}

pub fn parse_zones(v: &Value) -> Vec<Zone> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter(|z| z.get("active").map_or(true, |a| a.as_bool().unwrap_or(true) || a.as_i64() == Some(1)))
        .filter_map(|z| {
            Some(Zone {
                id: z.get("id")?.as_i64()?,
                name: z.get("name")?.as_str()?.to_string(),
                at: (z.get("lat").and_then(f64_of)?, z.get("lng").and_then(f64_of)?),
                radius_m: z.get("radius_m").and_then(f64_of)?,
            })
        })
        .collect()
}

/// The nearest place whose circle contains the point.
pub fn place_at<'a>(places: &'a [Place], p: (f64, f64)) -> Option<(&'a Place, f64)> {
    places
        .iter()
        .map(|pl| (pl, haversine_m(pl.at, p)))
        .filter(|(pl, d)| *d <= pl.radius_m)
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
}

pub fn zone_at(zones: &[Zone], p: (f64, f64)) -> Option<&Zone> {
    zones.iter().find(|z| haversine_m(z.at, p) <= z.radius_m)
}

pub async fn places(pool: &sqlx::PgPool) -> Vec<Place> {
    let mut out = Vec::new();
    for (kind, table, default_r) in [("terminal", "terminals", TERMINAL_RADIUS_M), ("drop_off_point", "drop_off_points", DROPOFF_RADIUS_M)] {
        let rows: Result<Vec<(String, f64, f64, Option<f64>)>, _> = sqlx::query_as(&format!(
            "SELECT name, latitude::float8, longitude::float8, radius_m::float8
               FROM {table}
              WHERE deleted_at IS NULL AND latitude IS NOT NULL AND longitude IS NOT NULL
                AND latitude <> 0 AND longitude <> 0"
        ))
        .fetch_all(pool)
        .await;
        // A missing table (older schema) just means no places of that kind.
        for (name, lat, lng, r) in rows.unwrap_or_default() {
            out.push(Place { kind, name, at: (lat, lng), radius_m: r.filter(|r| *r > 0.0).unwrap_or(default_r) });
        }
    }
    out
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct CarRow {
    car_id: i64,
    plate: String,
    operating_company: Option<String>,
    operating_area: Option<String>,
    etit_car_id: Option<String>,
    driver: Option<String>,
}

async fn cars(pool: &sqlx::PgPool, car_id: Option<i64>, company: Option<&str>) -> Result<Vec<CarRow>, String> {
    sqlx::query_as(
        "SELECT c.id::bigint AS car_id, c.car_no_plate AS plate, c.operating_company, c.operating_area,
                NULLIF(c.etit_car_id, '') AS etit_car_id, d.name AS driver
           FROM cars c LEFT JOIN drivers d ON d.id = c.driver_id
          WHERE c.deleted_at IS NULL
            AND ($1::bigint IS NULL OR c.id = $1)
            AND ($2::text IS NULL OR c.operating_company = $2)
          ORDER BY c.operating_company, c.car_no_plate",
    )
    .bind(car_id)
    .bind(company)
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())
}

/// Drivers logged in on a tablet now, by car.
async fn sessions(pool: &sqlx::PgPool) -> HashMap<i64, (String, Option<DateTime<Utc>>)> {
    let rows: Result<Vec<(i64, String, Option<DateTime<Utc>>)>, _> = sqlx::query_as(
        "SELECT s.car_id::bigint, d.name, s.start_time
           FROM driver_sessions s JOIN drivers d ON d.id = s.driver_id
          WHERE s.end_time IS NULL AND s.deleted_at IS NULL",
    )
    .fetch_all(pool)
    .await;
    rows.unwrap_or_default().into_iter().map(|(c, n, t)| (c, (n, t))).collect()
}

async fn live(ctx: &ToolCtx<'_>) -> Result<HashMap<String, Fix>, String> {
    let a = &ctx.adv;
    let v = upstream::etit_get(&a.http, &a.cfg, ctx.token, "/vehicles/live", &[])
        .await
        .map_err(|e| format!("live positions unavailable: {e}"))?;
    Ok(parse_live(&v))
}

async fn zones(ctx: &ToolCtx<'_>) -> Result<Vec<Zone>, String> {
    let a = &ctx.adv;
    let v = upstream::etit_get(&a.http, &a.cfg, ctx.token, "/zones", &[("active", "true".into())])
        .await
        .map_err(|e| format!("dead zones unavailable: {e}"))?;
    Ok(parse_zones(&v))
}

/// A truck's live position, if its tracker has a fix.
pub async fn position(ctx: &ToolCtx<'_>, car_id: i64) -> Option<((f64, f64), Option<DateTime<Utc>>)> {
    let car = cars(ctx.pool, Some(car_id), None).await.ok()?.into_iter().next()?;
    let fixes = live(ctx).await.ok()?;
    let fix = fixes.get(car.etit_car_id.as_deref()?)?;
    Some((fix.at, fix.ts))
}

fn describe(car: &CarRow, fix: Option<&Fix>, places: &[Place], zones: &[Zone], session: Option<&(String, Option<DateTime<Utc>>)>, now: DateTime<Utc>) -> Value {
    let mut out = json!({
        "car_id": car.car_id,
        "plate": car.plate,
        "operating_company": car.operating_company,
        "operating_area": car.operating_area,
        "assigned_driver": car.driver,
        "driver_on_tablet": session.map(|s| s.0.clone()),
    });
    let Some(etit) = car.etit_car_id.as_deref() else {
        out["tracking"] = json!("untracked (no ETIT tracker linked)");
        return out;
    };
    let Some(fix) = fix else {
        out["tracking"] = json!("no live data from the tracker");
        out["etit_id"] = json!(etit);
        return out;
    };
    let age = fix.ts.map(|t| (now - t).num_minutes());
    let mut status = status_name(fix.status);
    if status != "offline" && age.is_some_and(|m| m > NO_SIGNAL_AFTER_MIN) {
        status = "no_signal";
    }
    out["status"] = json!(status);
    out["speed_kmh"] = json!(fix.speed.round());
    out["last_fix_cairo"] = json!(fix.ts.as_ref().map(cairo));
    out["fix_age_min"] = json!(age);
    out["position"] = json!({ "lat": fix.at.0, "lng": fix.at.1 });
    if let Some(e) = &fix.event {
        out["last_event"] = json!(e);
    }
    out["at_place"] = match place_at(places, fix.at) {
        Some((p, d)) => json!({ "kind": p.kind, "name": p.name, "distance_m": d.round() }),
        None => Value::Null,
    };
    if let Some(z) = zone_at(zones, fix.at) {
        out["in_dead_zone"] = json!({ "zone_id": z.id, "name": z.name });
    }
    out
}

/// `fleet_status`: every truck's live state, place, dead-zone presence and
/// driver, plus counts.
pub async fn fleet_status(ctx: &ToolCtx<'_>, args: &Value) -> Result<Value, String> {
    let car_id = args.get("car_id").and_then(Value::as_i64);
    let company = args.get("company").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let want = args.get("status").and_then(Value::as_str);
    let (cars, fixes, zones, places, sessions) = (
        cars(ctx.pool, car_id, company).await?,
        live(ctx).await,
        zones(ctx).await,
        places(ctx.pool).await,
        sessions(ctx.pool).await,
    );
    let mut notes = Vec::new();
    let fixes = fixes.unwrap_or_else(|e| {
        notes.push(e);
        HashMap::new()
    });
    let zones = zones.unwrap_or_else(|e| {
        notes.push(e);
        Vec::new()
    });
    let now = Utc::now();
    let mut trucks: Vec<Value> = cars
        .iter()
        .map(|c| describe(c, c.etit_car_id.as_ref().and_then(|e| fixes.get(e)), &places, &zones, sessions.get(&c.car_id), now))
        .collect();
    let mut counts: HashMap<String, i64> = HashMap::new();
    for t in &trucks {
        let k = t["status"].as_str().map(str::to_string).unwrap_or_else(|| "untracked_or_no_data".into());
        *counts.entry(k).or_default() += 1;
    }
    if let Some(w) = want {
        trucks.retain(|t| t["status"] == w || (w == "in_dead_zone" && t.get("in_dead_zone").is_some()));
    }
    Ok(json!({
        "as_of_cairo": cairo(&now),
        "counts": counts,
        "trucks_in_dead_zones": trucks.iter().filter(|t| t.get("in_dead_zone").is_some()).count(),
        "trucks": trucks,
        "notes": notes,
    }))
}

/// `dead_zones`: the zones, who is inside one now, and (for a date range) the
/// recorded entries from a report-only scan.
pub async fn dead_zones(ctx: &ToolCtx<'_>, args: &Value) -> Result<Value, String> {
    let zones = zones(ctx).await?;
    let list: Vec<Value> = zones
        .iter()
        .map(|z| json!({ "id": z.id, "name": z.name, "lat": z.at.0, "lng": z.at.1, "radius_m": z.radius_m }))
        .collect();
    let cars = cars(ctx.pool, None, None).await?;
    let plate_of: HashMap<String, (i64, String)> = cars
        .iter()
        .filter_map(|c| c.etit_car_id.clone().map(|e| (e, (c.car_id, c.plate.clone()))))
        .collect();

    let mut inside_now = Vec::new();
    if let Ok(fixes) = live(ctx).await {
        for (etit, fix) in &fixes {
            if let Some(z) = zone_at(&zones, fix.at) {
                let (car_id, plate) = plate_of.get(etit).cloned().unwrap_or((0, etit.clone()));
                inside_now.push(json!({ "car_id": car_id, "plate": plate, "zone": z.name, "since_fix_cairo": fix.ts.as_ref().map(cairo) }));
            }
        }
    }

    let mut out = json!({
        "what": "Dead zones are restricted areas trucks must not enter (circles set on the Dead Zones page). A violation is a stay inside one of at least 2 minutes.",
        "zones": list,
        "inside_now": inside_now,
    });
    let from = args.get("from").and_then(Value::as_str);
    let to = args.get("to").and_then(Value::as_str);
    if let (Some(from), Some(to)) = (from, to) {
        let a = &ctx.adv;
        let resp = a
            .http
            .post(format!("{}/api/v1/zones/scan", a.cfg.etit_proxy_url))
            .bearer_auth(&ctx.token.0)
            // Always a range: an empty body is the alerting round.
            .json(&json!({ "from": from, "to": to }))
            .send()
            .await
            .map_err(|e| format!("zone scan unreachable: {e}"))?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(format!("zone scan answered {status}: {body}"));
        }
        let only_car = args.get("car_id").and_then(Value::as_i64);
        let violations: Vec<Value> = body["violations"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| {
                let etit = v["vehicle_id"].as_str().unwrap_or("");
                let (car_id, plate) = plate_of.get(etit).cloned().unwrap_or((0, v["codename"].as_str().unwrap_or(etit).to_string()));
                if only_car.is_some_and(|c| c != car_id) {
                    return None;
                }
                Some(json!({
                    "car_id": car_id, "plate": plate, "zone": v["zone_name"],
                    "entered_cairo": v["entry_local"], "left_cairo": v["exit_local"],
                    "dwell_min": v["dwell_secs"].as_f64().map(|s| (s / 60.0).round()),
                }))
            })
            .collect();
        out["range"] = json!({ "from": from, "to": to, "days_scanned": body["days_scanned"], "vehicles_scanned": body["vehicles_scanned"] });
        out["violations"] = json!(violations);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_follows_the_numeric_code() {
        assert_eq!(status_name(1), "moving");
        assert_eq!(status_name(3), "stopped");
        assert_eq!(status_name(13), "idling");
        assert_eq!(status_name(12), "offline");
        assert_eq!(status_name(18), "geofence_violation");
        assert_eq!(status_name(99), "unknown");
    }

    #[test]
    fn live_rows_without_coordinates_are_dropped() {
        let v = json!([
            { "id": "a", "lat": 30.1, "lng": 31.2, "speed": 70, "status": 1, "timestamp": "2026-10-25T07:18:00Z" },
            { "id": "b", "lat": 0, "lng": 0, "status": 12 }
        ]);
        let m = parse_live(&v);
        assert_eq!(m.len(), 1);
        assert_eq!(m["a"].status, 1);
        assert_eq!(cairo(&m["a"].ts.unwrap()), "2026-10-25 10:18");
    }

    #[test]
    fn a_stale_fix_is_no_signal() {
        let car = CarRow { car_id: 1, plate: "1234".into(), operating_company: None, operating_area: None, etit_car_id: Some("a".into()), driver: None };
        let now = Utc::now();
        let fix = Fix { at: (30.0, 31.0), speed: 0.0, status: 3, ts: Some(now - chrono::Duration::minutes(45)), event: None };
        assert_eq!(describe(&car, Some(&fix), &[], &[], None, now)["status"], "no_signal");
        let fresh = Fix { ts: Some(now), ..fix };
        assert_eq!(describe(&car, Some(&fresh), &[], &[], None, now)["status"], "stopped");
    }

    #[test]
    fn places_and_zones_are_circles() {
        let places = vec![
            Place { kind: "terminal", name: "Mostorod".into(), at: (30.1500, 31.2900), radius_m: 500.0 },
            Place { kind: "drop_off_point", name: "Benha".into(), at: (30.4600, 31.1800), radius_m: 300.0 },
        ];
        assert_eq!(place_at(&places, (30.1520, 31.2910)).unwrap().0.name, "Mostorod");
        assert!(place_at(&places, (30.30, 31.20)).is_none());
        let zones = vec![Zone { id: 7, name: "Old quarry".into(), at: (29.9, 31.5), radius_m: 1000.0 }];
        assert_eq!(zone_at(&zones, (29.905, 31.5)).unwrap().id, 7);
        assert!(zone_at(&zones, (29.95, 31.5)).is_none());
    }
}
