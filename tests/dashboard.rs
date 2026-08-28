//! The dashboard endpoint, end to end: real routes, real gates, real tables.
//!
//! The entry point earns a stricter test than most pages — it is the first
//! request every session makes, and a payload that lies here lies everywhere.

mod support;

use actix_web::{test, web, App};
use sqlx::PgPool;
use support::seed_trip_fixture;

async fn db(name: &str) -> PgPool {
    let pool = support::fresh_db(name).await;
    // The banksms schema, exactly as production got it.
    apex::boot::run_banksms_migrations(&pool)
        .await
        .expect("banksms migrations");
    seed_trip_fixture(&pool).await;

    sqlx::raw_sql(
        "-- Give the fixture cars a fleet identity: two tracked, one service
         -- vehicle with the empty-string etit id production actually stores.
         INSERT INTO cars (car_no_plate, etit_car_id) VALUES
            ('PA-A', 'etit-aaaa'), ('WA-C', 'etit-cccc'), ('SVC 9001', '');
         -- Wire the fixture trips to their car rows so last-trip resolves.
         UPDATE trips SET car_id = (SELECT id FROM cars WHERE car_no_plate = trips.car_no_plate)
          WHERE car_no_plate IN ('PA-A','WA-C');

         -- Money out, in-window and out-of-window, plus a split parent that
         -- must NOT double-count, plus an uncategorised (unreviewed) transfer.
         INSERT INTO banksms.transactions (source, direction, amount, currency, occurred_at, category)
         VALUES ('manual','out', 1000, 'EGP', '2025-05-10T09:00:00Z', 'Fuel'),
                ('manual','out',  250, 'EGP', '2025-05-11T09:00:00Z', 'Parts'),
                ('manual','out', 9999, 'EGP', '2024-01-01T09:00:00Z', 'Fuel'),
                ('manual','in',   500, 'EGP', '2025-05-12T09:00:00Z', NULL);
         WITH parent AS (
           INSERT INTO banksms.transactions (source, direction, amount, currency, occurred_at, category, split_at)
           VALUES ('manual','out', 600, 'EGP', '2025-05-13T09:00:00Z', 'Maintenance', now())
           RETURNING id)
         INSERT INTO banksms.transactions (source, direction, amount, currency, occurred_at, category, parent_id)
         SELECT 'split','out', 300, 'EGP', '2025-05-13T09:00:00Z'::timestamptz, 'Maintenance', id FROM parent
         UNION ALL
         SELECT 'split','out', 300, 'EGP', '2025-05-13T09:00:00Z'::timestamptz, 'Labor', id FROM parent;

         -- An unpaid advance and a paid one; only the unpaid counts.
         INSERT INTO drivers (id, name) VALUES (901, 'سائق تجريبي');
         INSERT INTO loans (driver_id, amount, method, date, is_paid, kind)
         VALUES (901, 750, 'cash', '2025-05-01', false, 'advance'),
                (901, 400, 'cash', '2025-04-01', true,  'advance');",
    )
    .execute(&pool)
    .await
    .expect("dashboard seed");
    pool
}

macro_rules! app_of {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .configure(apex::handlers::configure_api_v1),
        )
        .await
    };
}

macro_rules! get_json {
    ($app:expr, $permission:expr, $uri:expr) => {{
        let token = support::token_with_permission(50, $permission);
        let req = test::TestRequest::get()
            .uri($uri)
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request();
        let v: serde_json::Value = test::call_and_read_body_json(&$app, req).await;
        v
    }};
}

/// The whole contract in one pass: month totals, the money gate in both
/// directions, split-parent exclusion, fleet identity, and exceptions.
#[actix_web::test]
async fn the_payload_says_what_the_tables_say() {
    support::init();
    let pool = db("apex_dash_main").await;
    let app = app_of!(pool);

    let body = get_json!(app, 4, "/api/v1/dashboard?month=2025-05");

    // Month totals: the fixture's May trips. Logical trips — the Watanya
    // three-container group counts once.
    let month = &body["month"];
    assert!(month["trips"].as_i64().unwrap() > 0);
    assert!(month["litres"].as_i64().unwrap() > 0);

    // Cash out: 1000 + 250 + the split PARENT's 600 counted once via its
    // children (300 Maintenance + 300 Labor) — never both layers, and never
    // the out-of-window 9999.
    let money = &body["money"];
    assert_eq!(money["cash_out"].as_str().unwrap(), "1850.0000");

    // Advances: only the unpaid row.
    assert_eq!(money["advances_outstanding"].as_str().unwrap(), "750.00");
    assert_eq!(money["advances_count"].as_i64().unwrap(), 1);

    // Revenue is a decimal string and positive — the exact figure is pinned by
    // the revenue parity suite; here we only assert it arrived as money should.
    let revenue = money["revenue"].as_str().unwrap();
    assert!(revenue.parse::<f64>().unwrap() > 0.0, "revenue {revenue}");

    // Fleet: identity split into digits + letters, service vehicle flagged by
    // a null etit_id (normalised from production's empty string).
    let fleet = body["fleet"].as_array().unwrap();
    let svc = fleet.iter().find(|f| f["plate_no"] == "9001").expect("service vehicle");
    assert!(svc["etit_id"].is_null());
    assert_eq!(svc["plate_ar"], "SVC");
    let tracked = fleet.iter().find(|f| f["etit_id"] == "etit-aaaa").expect("tracked");
    assert_eq!(tracked["plate_no"], "PA-A".replace("PA-A", "PA-A"));

    // Exceptions: the fixture's unmapped-route trips surface, and the
    // unreviewed incoming transfer does too. Every exception carries a href.
    let exceptions = body["exceptions"].as_array().unwrap();
    assert!(exceptions.iter().any(|e| e["key"] == "trips_earning_zero"));
    assert!(exceptions.iter().any(|e| e["key"] == "transactions_unreviewed"
        && e["count"].as_i64() == Some(1)));
    assert!(exceptions.iter().all(|e| e["href"].as_str().is_some()));
}

/// Below permission 4 the money key does not exist. Not zeroed, not nulled —
/// absent, exactly like the trips list's revenue fields.
#[actix_web::test]
async fn money_is_absent_below_permission_4() {
    support::init();
    let pool = db("apex_dash_gate").await;
    let app = app_of!(pool);

    let body = get_json!(app, 1, "/api/v1/dashboard?month=2025-05");
    assert!(body.get("money").is_none(), "money leaked: {body}");
    // But the operational half still works for a dispatcher.
    assert!(body["month"]["trips"].as_i64().unwrap() > 0);
    assert!(!body["fleet"].as_array().unwrap().is_empty());

    // And the money drawers refuse outright at the route.
    for uri in [
        "/api/v1/dashboard/revenue",
        "/api/v1/dashboard/cash-out",
        "/api/v1/dashboard/advances",
    ] {
        let token = support::token_with_permission(51, 3);
        let req = test::TestRequest::get()
            .uri(uri)
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request();
        // A middleware rejection surfaces as a service Err; a handler-level
        // one arrives as a 403 response. Either way, no data.
        match test::try_call_service(&app, req).await {
            Err(_) => {}
            Ok(resp) => assert_eq!(
                resp.status(),
                actix_web::http::StatusCode::FORBIDDEN,
                "{uri} answered permission 3"
            ),
        }
    }
}

/// The drawers carry what their cards promise.
#[actix_web::test]
async fn drawers_open_onto_real_detail() {
    support::init();
    let pool = db("apex_dash_drawers").await;
    let app = app_of!(pool);

    let rev = get_json!(app, 4, "/api/v1/dashboard/revenue?month=2025-05");
    assert!(!rev["companies"].as_array().unwrap().is_empty());
    assert!(!rev["daily"].as_array().unwrap().is_empty());

    let cash = get_json!(app, 4, "/api/v1/dashboard/cash-out?month=2025-05");
    let cats = cash["by_category"].as_array().unwrap();
    // The split's Labor child appears as its own category line.
    assert!(cats.iter().any(|c| c["name"] == "Labor"));
    let largest = cash["largest"].as_array().unwrap();
    assert!(largest.len() <= 5 && !largest.is_empty());
    // Split parents never appear among the largest payments.
    assert!(largest.iter().all(|p| p["amount"].as_str() != Some("600")));

    let trips = get_json!(app, 4, "/api/v1/dashboard/trips?month=2025-05");
    assert!(!trips["companies"].as_array().unwrap().is_empty());
    assert!(!trips["daily"].as_array().unwrap().is_empty());

    let adv = get_json!(app, 4, "/api/v1/dashboard/advances");
    let parties = adv["parties"].as_array().unwrap();
    assert_eq!(parties.len(), 1);
    assert_eq!(parties[0]["name"], "سائق تجريبي");
    assert_eq!(parties[0]["total"], "750.00");
}

/// MessagePack carries the identical payload — including the ABSENCE of the
/// money key below permission 4, which is the gate itself.
#[actix_web::test]
async fn msgpack_matches_json_including_absences() {
    support::init();
    let pool = db("apex_dash_msgpack").await;
    let app = app_of!(pool);

    for permission in [1, 4] {
        let json = get_json!(app, permission, "/api/v1/dashboard?month=2025-05");

        let token = support::token_with_permission(52, permission);
        let req = test::TestRequest::get()
            .uri("/api/v1/dashboard?month=2025-05&format=msgpack")
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.headers().get("content-type").unwrap(), "application/msgpack");
        let packed: serde_json::Value =
            rmp_serde::from_slice(&test::read_body(resp).await).expect("decode");

        // as_of differs between the two calls by construction; everything else
        // must match key for key.
        let strip = |mut v: serde_json::Value| {
            v.as_object_mut().unwrap().remove("as_of");
            v
        };
        assert_eq!(strip(packed), strip(json), "diverged at permission {permission}");
    }
}
