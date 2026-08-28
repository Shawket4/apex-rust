//! The trips list, tested against the same fixture the revenue parity suite
//! uses — so a disagreement between the list and the statistics page shows up
//! here rather than in front of a user.

mod support;

use apex::db::trip_queries::{list_trips, TripListFilters, MAX_LIMIT};
use apex::models::trip_list::TripListRow;
use sqlx::PgPool;
use support::seed_trip_fixture;

const FROM: &str = "2025-05-01";
const TO: &str = "2025-06-30";

/// Filters over the whole fixture window, one page big enough to hold it.
fn all() -> TripListFilters {
    TripListFilters {
        page: 1,
        limit: MAX_LIMIT,
        from: Some(FROM.into()),
        to: Some(TO.into()),
        ..Default::default()
    }
    .normalized()
}

async fn db(name: &str) -> PgPool {
    let pool = support::fresh_db(name).await;
    seed_trip_fixture(&pool).await;
    pool
}

fn find<'a>(rows: &'a [TripListRow], receipt_no: &str) -> &'a TripListRow {
    rows.iter()
        .find(|r| r.receipt_no == receipt_no)
        .unwrap_or_else(|| panic!("receipt {receipt_no} missing from the list"))
}

/* ------------------------------------------------------------------------ */
/* Revenue                                                                   */
/* ------------------------------------------------------------------------ */

/// The reason the endpoint exists: a trip carries the revenue its own company's
/// formula produces, computed by the same module statistics uses.
#[tokio::test]
async fn rows_carry_revenue_from_their_company_formula() {
    let pool = db("apex_trips_revenue").await;
    let (rows, _) = list_trips(&pool, &all(), true).await.unwrap();

    // Petrol Arrows: volume * fee / 1000 = 40000 * 30.5 / 1000.
    let pa = find(&rows, "PA1");
    assert_eq!(pa.revenue, Some(1220.0));
    // No car rental for Petrol Arrows, and no VAT either.
    assert_eq!(pa.allocated_rental, Some(0.0));
    assert_eq!(pa.allocated_vat, Some(0.0));
    assert_eq!(pa.allocated_total, Some(1220.0));

    // Watanya band 2: 40000 * 122.1 / 1000, then 14% VAT.
    let wa = find(&rows, "WA2");
    assert_eq!(wa.revenue, Some(4884.0));
    assert_eq!(wa.allocated_rental, Some(0.0));
    assert!((wa.allocated_vat.unwrap() - 683.76).abs() < 0.01);

    // An unmapped route earns nothing rather than NULL, and still appears.
    let unmapped = find(&rows, "WA4");
    assert_eq!(unmapped.revenue, Some(0.0));
    assert_eq!(unmapped.fee, 0.0);
    assert_eq!(unmapped.distance, 0.0);
}

/// A Petromin car that ran twice in one day earns ONE car-day between the two
/// trips, so each carries half of it. Charging each row the full 2,000 is the
/// obvious way to get this wrong.
#[tokio::test]
async fn a_shared_car_day_is_split_not_duplicated() {
    let pool = db("apex_trips_carday").await;
    let (rows, _) = list_trips(&pool, &all(), true).await.unwrap();

    // PM1 and PM2: same car, same day. 2000 / 2 rows.
    assert_eq!(find(&rows, "PM1").allocated_rental, Some(1000.0));
    assert_eq!(find(&rows, "PM2").allocated_rental, Some(1000.0));
    // PM3 is that car's only trip on the 19th, so it carries the whole day.
    assert_eq!(find(&rows, "PM3").allocated_rental, Some(2000.0));
    // PM4 is a different car on the 18th — its own day, not a share of PM1's.
    assert_eq!(find(&rows, "PM4").allocated_rental, Some(2000.0));
}

/// Below permission 4 the money fields are absent from the payload, not zero.
/// A zero would read as "this trip earned nothing", which is a different and
/// wrong claim.
#[tokio::test]
async fn revenue_is_absent_not_zero_without_permission() {
    let pool = db("apex_trips_noperm").await;
    let (rows, _) = list_trips(&pool, &all(), false).await.unwrap();

    assert!(!rows.is_empty());
    for row in &rows {
        assert!(row.revenue.is_none(), "revenue leaked on {}", row.receipt_no);
        assert!(row.allocated_rental.is_none());
        assert!(row.allocated_vat.is_none());
        assert!(row.allocated_total.is_none());
    }

    // Serialisation must drop the keys entirely, not emit nulls.
    let json = serde_json::to_string(&rows[0]).unwrap();
    assert!(!json.contains("revenue"), "revenue key present: {json}");
    assert!(!json.contains("allocated_"), "allocated key present: {json}");
}

/// A search must not change what a trip earned. The allocation divides a shared
/// cost by the rows sharing it, so scoping it to the search results would make
/// PM1 jump from 1,000 to 2,000 just because its sibling was filtered out.
#[tokio::test]
async fn searching_does_not_change_what_a_trip_earned() {
    let pool = db("apex_trips_search_stable").await;

    let (all_rows, _) = list_trips(&pool, &all(), true).await.unwrap();
    let baseline = find(&all_rows, "PM1").allocated_rental;

    let narrowed = TripListFilters {
        search: Some("PM1".into()),
        ..all()
    };
    let (found, total) = list_trips(&pool, &narrowed, true).await.unwrap();

    assert_eq!(total, 1, "search should match exactly one row");
    assert_eq!(
        find(&found, "PM1").allocated_rental,
        baseline,
        "a trip's allocated rental changed because of a search term"
    );
}

/* ------------------------------------------------------------------------ */
/* Containers                                                                */
/* ------------------------------------------------------------------------ */

/// A page that touches one container of a multi-container trip must bring all
/// of its siblings, even the ones past the page boundary. Showing two of three
/// containers displays a trip total that is simply wrong.
#[tokio::test]
async fn a_page_brings_every_sibling_container() {
    let pool = db("apex_trips_siblings").await;

    // One row per page, walked until the container group appears.
    let mut found_group_at = None;
    for page in 1..=40 {
        let filters = TripListFilters {
            page,
            limit: 1,
            ..all()
        }
        .normalized();
        let (rows, _) = list_trips(&pool, &filters, true).await.unwrap();
        if rows.iter().any(|r| r.parent_trip_id == Some(9001)) {
            let containers: Vec<_> = rows
                .iter()
                .filter(|r| r.parent_trip_id == Some(9001))
                .collect();
            assert_eq!(
                containers.len(),
                3,
                "page of 1 returned {} of the 3 containers",
                containers.len()
            );
            found_group_at = Some(page);
            break;
        }
    }
    assert!(
        found_group_at.is_some(),
        "the container group never appeared while paging"
    );
}

/// Containers carry their parent header, and the parent carries its scanned
/// receipt batch — the nested graph the dashboard renders.
#[tokio::test]
async fn containers_carry_their_parent_and_its_receipts() {
    let pool = db("apex_trips_parent").await;

    sqlx::raw_sql(
        "INSERT INTO drivers (id, name) VALUES (77, 'Container Driver');
         INSERT INTO receipt_batches (id, driver_id, status, assigned_to_trip_id)
         VALUES (500, 77, 'assigned', 9001);
         INSERT INTO receipts (batch_id, image_path) VALUES
            (500, 'a.jpg'), (500, 'b.jpg');",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (rows, _) = list_trips(&pool, &all(), true).await.unwrap();
    let container = find(&rows, "WAC1");

    let parent = container.parent_trip.as_ref().expect("parent header missing");
    assert_eq!(parent.id, 9001);
    assert_eq!(parent.car_no_plate, "WA-C");

    let batch = parent.receipt_batch.as_ref().expect("receipt batch missing");
    assert_eq!(batch.receipts.len(), 2);
    assert_eq!(
        batch.driver.as_ref().and_then(|d| d.name.as_deref()),
        Some("Container Driver")
    );

    // A standalone trip has no parent to carry.
    assert!(find(&rows, "PA1").parent_trip.is_none());
}

/* ------------------------------------------------------------------------ */
/* Receipt steps and filters                                                 */
/* ------------------------------------------------------------------------ */

/// Receipt-status filtering keys off the LATEST step, not merely the presence
/// of one: a receipt that reached the garage and then the office is in the
/// office, and must not answer to `in_garage`.
#[tokio::test]
async fn receipt_status_follows_the_latest_step() {
    let pool = db("apex_trips_receipt_status").await;

    // PA1 went to the garage and then on to the office. PA2 is still in the
    // garage. PA3 has no steps at all.
    let pa1: i32 = sqlx::query_scalar("SELECT id FROM trips WHERE receipt_no = 'PA1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let pa2: i32 = sqlx::query_scalar("SELECT id FROM trips WHERE receipt_no = 'PA2'")
        .fetch_one(&pool)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO receipt_steps (trip_id, location, received_by, received_at, step_order)
         VALUES ($1, 'Garage', 'a', TIMESTAMP '2025-05-05 08:00', 1),
                ($1, 'Office', 'b', TIMESTAMP '2025-05-06 08:00', 2),
                ($2, 'Garage', 'c', TIMESTAMP '2025-05-06 09:00', 1)",
    )
    .bind(pa1 as i64)
    .bind(pa2 as i64)
    .execute(&pool)
    .await
    .unwrap();

    let status = |s: &str| TripListFilters {
        receipt_status: Some(s.into()),
        ..all()
    };

    let (office, _) = list_trips(&pool, &status("in_office"), true).await.unwrap();
    assert_eq!(office.len(), 1);
    assert_eq!(office[0].receipt_no, "PA1");
    // Both steps come back on the row, in order.
    assert_eq!(office[0].receipt_steps.len(), 2);
    assert_eq!(office[0].receipt_steps[0].location, "Garage");

    let (garage, _) = list_trips(&pool, &status("in_garage"), true).await.unwrap();
    assert_eq!(garage.len(), 1, "a receipt that moved on is still 'in garage'");
    assert_eq!(garage[0].receipt_no, "PA2");

    let (pending, _) = list_trips(&pool, &status("pending"), true).await.unwrap();
    assert!(pending.iter().all(|r| r.receipt_steps.is_empty()));
    assert!(pending.iter().any(|r| r.receipt_no == "PA3"));
}

/// The `missing_data` filter finds the rows FalconGo marks with its
/// "not registered" sentinel.
#[tokio::test]
async fn missing_data_filter_finds_unregistered_rows() {
    let pool = db("apex_trips_missing").await;
    sqlx::raw_sql(
        "UPDATE trips SET driver_name = 'غير مسجل' WHERE receipt_no = 'PA1';
         UPDATE trips SET drop_off_point = 'غير مسجل' WHERE receipt_no = 'PA2';",
    )
    .execute(&pool)
    .await
    .unwrap();

    let missing = |kind: &str| TripListFilters {
        missing_data: Some(kind.into()),
        ..all()
    };

    let (driver, _) = list_trips(&pool, &missing("driver"), true).await.unwrap();
    assert_eq!(driver.len(), 1);
    assert_eq!(driver[0].receipt_no, "PA1");

    let (route, _) = list_trips(&pool, &missing("route"), true).await.unwrap();
    assert_eq!(route.len(), 1);
    assert_eq!(route[0].receipt_no, "PA2");

    let (any, total) = list_trips(&pool, &missing("any"), true).await.unwrap();
    assert_eq!(total, 2);
    assert_eq!(any.len(), 2);
}

/* ------------------------------------------------------------------------ */
/* Pagination                                                                */
/* ------------------------------------------------------------------------ */

/// Paging must cover every row exactly once, and the soft-deleted row must
/// never appear on any page.
#[tokio::test]
async fn paging_covers_every_row_exactly_once() {
    let pool = db("apex_trips_paging").await;

    let (_, total) = list_trips(&pool, &all(), true).await.unwrap();
    assert!(total > 10, "fixture should span several pages");

    let mut seen: Vec<i64> = Vec::new();
    for page in 1..=((total / 7) + 2) {
        let filters = TripListFilters {
            page,
            limit: 7,
            ..all()
        }
        .normalized();
        let (rows, page_total) = list_trips(&pool, &filters, true).await.unwrap();
        assert_eq!(page_total, total, "total drifted between pages");
        seen.extend(rows.iter().map(|r| r.id));
    }

    // Siblings are spliced into whichever page touches their parent, so the
    // same container can legitimately arrive twice across pages.
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len() as i64, total, "paging missed or invented rows");

    let deleted: Option<i32> =
        sqlx::query_scalar("SELECT id FROM trips WHERE receipt_no = 'DEL1'")
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert!(
        !seen.contains(&(deleted.unwrap() as i64)),
        "a soft-deleted trip was listed"
    );
}

/// A caller asking for an enormous page gets the cap, not the whole table.
/// FalconGo had no ceiling here at all.
#[tokio::test]
async fn page_size_is_capped() {
    let filters = TripListFilters {
        page: 0,
        limit: 100_000,
        ..Default::default()
    }
    .normalized();
    assert_eq!(filters.limit, MAX_LIMIT);
    assert_eq!(filters.page, 1, "page 0 is clamped to the first page");
}

/* ------------------------------------------------------------------------ */
/* One day is one day                                                        */
/* ------------------------------------------------------------------------ */

/// A car that serves two terminals on the same date has worked ONE day, and is
/// rented for one day.
///
/// This did not used to hold. The statistics queries counted working days per
/// (terminal, car, month), so such a date was credited to each terminal and the
/// fleet was billed a day it had not earned — live in production, where car
/// "ف ع ص 4381" ran Alex and Suez on 2025-06-05.
///
/// The single rental is now split between the terminals in proportion to the
/// days each saw, so per-terminal reporting still works, the shares add back to
/// one rental, and the trips list agrees with all of it.
#[tokio::test]
async fn a_car_at_two_terminals_in_one_day_is_rented_for_one_day() {
    use apex::db::revenue::TAQA_RENTAL_PER_DAY;
    use apex::db::stats_queries::get_taqa_stats;

    let pool = support::fresh_db("apex_trips_two_terminals").await;
    sqlx::raw_sql(
        "INSERT INTO fee_mappings (company, terminal, drop_off_point, distance, fee) VALUES
            ('TAQA','Alex','Site',100.0,0), ('TAQA','Suez','Site',100.0,0);
         -- One car, ten days at Alex. On the tenth it also runs from Suez.
         INSERT INTO trips (company, terminal, drop_off_point, car_no_plate,
                            tank_capacity, date, receipt_no)
         SELECT 'TAQA','Alex','Site','SPLIT-1', 30000,
                to_char(DATE '2025-05-01' + (n || ' days')::interval, 'YYYY-MM-DD'),
                'A' || n
         FROM generate_series(0, 9) AS n;
         INSERT INTO trips (company, terminal, drop_off_point, car_no_plate,
                            tank_capacity, date, receipt_no)
         VALUES ('TAQA','Suez','Site','SPLIT-1', 30000, '2025-05-10', 'S1');",
    )
    .execute(&pool)
    .await
    .unwrap();

    let stats = get_taqa_stats(&pool, "2025-05-01", "2025-05-31", true)
        .await
        .unwrap();
    let stats_rental: f64 = stats.iter().filter_map(|d| d.car_rental).sum();

    // Ten distinct dates worked, so exactly one tapered rental for ten days --
    // NOT one for Alex's ten plus another for Suez's one.
    let expected = 43_000.0 - (28.0 - 10.0) * TAQA_RENTAL_PER_DAY;
    assert!(
        (stats_rental - expected).abs() < 0.01,
        "expected one rental of {expected}, got {stats_rental}"
    );

    // The old behaviour, for the record: a second full taper for Suez's single
    // day. If this ever passes again, the day is being billed twice.
    let double_billed = expected + (43_000.0 - (28.0 - 1.0) * TAQA_RENTAL_PER_DAY);
    assert!(
        (stats_rental - double_billed).abs() > 1.0,
        "the shared day is being billed to both terminals again"
    );

    // Both terminals still report a rental -- the day is split, not assigned to
    // whichever terminal happened to sort first -- and the parts add back up.
    let per_terminal: Vec<f64> = stats.iter().filter_map(|d| d.car_rental).collect();
    assert_eq!(per_terminal.len(), 2, "expected a row for Alex and for Suez");
    assert!(
        per_terminal.iter().all(|v| *v > 0.0),
        "a terminal that used the car was charged nothing: {per_terminal:?}"
    );
    assert!(
        (per_terminal.iter().sum::<f64>() - expected).abs() < 0.01,
        "the terminal shares do not add back to the one rental"
    );

    // And the trips list agrees with statistics, which is the contract.
    let filters = TripListFilters {
        page: 1,
        limit: MAX_LIMIT,
        company: Some("TAQA".into()),
        from: Some("2025-05-01".into()),
        to: Some("2025-05-31".into()),
        ..Default::default()
    }
    .normalized();
    let (rows, _) = list_trips(&pool, &filters, true).await.unwrap();
    let list_rental: f64 = rows.iter().filter_map(|r| r.allocated_rental).sum();
    assert!(
        (list_rental - stats_rental).abs() < 0.01,
        "list rental {list_rental} != statistics {stats_rental}"
    );
}

/* ------------------------------------------------------------------------ */
/* The HTTP surface                                                          */
/* ------------------------------------------------------------------------ */

/// The two gates, exercised through the real route rather than the query layer:
/// level 1 gets the list, level 4 gets the money, and an unauthenticated caller
/// gets neither.
#[actix_web::test]
async fn the_endpoint_gates_the_list_and_the_money_separately() {
    use actix_web::{test, web, App, ResponseError};

    support::init();
    let pool = db("apex_trips_http").await;
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .configure(apex::handlers::configure_api_v1),
    )
    .await;

    let get = |token: Option<String>| {
        let mut req = test::TestRequest::get().uri("/api/v1/trips?limit=50&from=2025-05-01&to=2025-06-30");
        if let Some(t) = token {
            req = req.insert_header(("Authorization", format!("Bearer {t}")));
        }
        req.to_request()
    };

    // No credentials at all. JwtAuth rejects by returning an Error rather than
    // a response, so this has to go through the fallible call.
    let err = test::try_call_service(&app, get(None))
        .await
        .expect_err("an anonymous caller got the trips list");
    assert_eq!(err.error_response().status(), 401);

    // A dispatcher: sees the list, not the money.
    let viewer = support::token_with_permission(11, 1);
    let body: serde_json::Value =
        test::call_and_read_body_json(&app, get(Some(viewer))).await;
    let rows = body["data"].as_array().expect("data array");
    assert!(!rows.is_empty(), "level 1 got an empty list");
    assert!(body["meta"]["total"].as_i64().unwrap() > 0);
    for row in rows {
        assert!(row.get("revenue").is_none(), "revenue reached level 1");
        assert!(row.get("allocated_total").is_none());
    }
    // The rest of the row is intact — this is not a stripped-down payload.
    assert!(rows[0].get("ID").is_some(), "gorm casing lost: {:?}", rows[0]);
    assert!(rows[0].get("receipt_no").is_some());
    assert!(rows[0].get("receipt_steps").is_some());

    // An admin: same rows, with the money.
    let admin = support::token_with_permission(12, 4);
    let body: serde_json::Value =
        test::call_and_read_body_json(&app, get(Some(admin))).await;
    let rows = body["data"].as_array().expect("data array");
    assert!(rows.iter().any(|r| r.get("revenue").is_some()),
            "level 4 got no revenue at all");
    assert!(rows.iter().any(|r| r.get("allocated_total").is_some()));

    // Level 3 is the statistics threshold and is deliberately NOT enough here.
    let manager = support::token_with_permission(13, 3);
    let body: serde_json::Value =
        test::call_and_read_body_json(&app, get(Some(manager))).await;
    for row in body["data"].as_array().unwrap() {
        assert!(
            row.get("revenue").is_none(),
            "level 3 saw per-trip revenue; statistics access is not list access"
        );
    }
}

/// The dashboard asks for MessagePack. The encoding must carry exactly what
/// JSON does — including the ABSENCE of the money fields, which is the whole
/// mechanism the permission gate relies on. A codec that turned a skipped
/// field into a null would quietly turn "not allowed to see" into
/// "earned nothing".
#[actix_web::test]
async fn msgpack_carries_the_same_payload_as_json() {
    use actix_web::{test, web, App};

    support::init();
    let pool = db("apex_trips_msgpack").await;
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .configure(apex::handlers::configure_api_v1),
    )
    .await;

    let fetch = |permission: i32, format: &str| {
        let token = support::token_with_permission(20, permission);
        test::TestRequest::get()
            .uri(&format!(
                "/api/v1/trips?limit=50&from=2025-05-01&to=2025-06-30&format={format}"
            ))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request()
    };

    for permission in [1, 4] {
        let json: serde_json::Value =
            test::call_and_read_body_json(&app, fetch(permission, "json")).await;

        let resp = test::call_service(&app, fetch(permission, "msgpack")).await;
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/msgpack"
        );
        let bytes = test::read_body(resp).await;
        let packed: serde_json::Value =
            rmp_serde::from_slice(&bytes).expect("decode msgpack");

        assert!(
            same_payload(&packed, &json, "$"),
            "msgpack and json disagree at permission {permission}"
        );

        // And the gate survived the encoding, not just the comparison.
        let rows = packed["data"].as_array().unwrap();
        assert!(!rows.is_empty());
        let has_money = rows.iter().any(|r| r.get("revenue").is_some());
        assert_eq!(
            has_money,
            permission >= 4,
            "money visibility wrong in msgpack at permission {permission}"
        );
        // Absent, never null.
        for row in rows {
            assert!(
                !row.get("revenue").is_some_and(|v| v.is_null()),
                "a skipped money field encoded as null"
            );
        }
    }
}

/// Structural equality, with floats compared to within a rounding step.
///
/// The two encodings genuinely differ in the last bit of an f64, and MessagePack
/// is the accurate one: JSON round-trips a float through decimal text, so
/// 13840.414285714285 comes back as 13840.414285714283. Asserting exact equality
/// would be asserting that JSON does not lose precision, which it does.
fn same_payload(a: &serde_json::Value, b: &serde_json::Value, path: &str) -> bool {
    use serde_json::Value;
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            match (x.as_f64(), y.as_f64()) {
                (Some(x), Some(y)) => {
                    let ok = (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0);
                    if !ok {
                        eprintln!("{path}: {x} != {y}");
                    }
                    ok
                }
                _ => x == y,
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y)
                    .enumerate()
                    .all(|(i, (a, b))| same_payload(a, b, &format!("{path}[{i}]")))
        }
        (Value::Object(x), Value::Object(y)) => {
            // Key sets must match exactly — an omitted field appearing in one
            // encoding and not the other is the failure this test exists for.
            if x.len() != y.len() {
                eprintln!("{path}: key counts differ ({} vs {})", x.len(), y.len());
                return false;
            }
            x.iter().all(|(k, v)| match y.get(k) {
                Some(other) => same_payload(v, other, &format!("{path}.{k}")),
                None => {
                    eprintln!("{path}.{k}: missing on one side");
                    false
                }
            })
        }
        _ => {
            let ok = a == b;
            if !ok {
                eprintln!("{path}: {a} != {b}");
            }
            ok
        }
    }
}

/// The route must still resolve when the banksms scopes are mounted alongside
/// it, in the order the binary mounts them.
///
/// This is the outage this test exists for. `/api/v1/trips` was registered in
/// its own `web::scope("/api/v1")` next to the existing one. actix matches the
/// first service whose prefix matches and does not fall through, so every
/// `/api/v1/...` request went to whichever scope came first and the trips route
/// was unreachable — 404 in production, green in CI, because the test mounted
/// its scope alone.
#[actix_web::test]
async fn the_route_resolves_alongside_the_banksms_scopes() {
    use actix_web::{test, web, App};

    support::init();
    let pool = db("apex_trips_scopes").await;
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            // Same order as main.rs: banksms first, then the /api/v1 surface.
            .configure(apex::api::configure)
            .configure(apex::handlers::configure_api_v1),
    )
    .await;

    let token = support::token_with_permission(30, 4);
    let req = test::TestRequest::get()
        .uri("/api/v1/trips?limit=5&from=2025-05-01&to=2025-06-30")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        200,
        "the trips route was shadowed by another /api/v1 scope"
    );

    // And the neighbours still answer, so the fix did not shadow them instead.
    let req = test::TestRequest::get()
        .uri("/api/v1/categories")
        .insert_header(("Authorization", format!("Bearer {}", support::token_with_permission(31, 4))))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_ne!(resp.status(), 404, "the banksms routes stopped resolving");
}

/// A day's rental is shared only by the trips on THAT day.
///
/// It used to be the month's rental divided by the month's trips, so one busy
/// day diluted every other trip the car made — a trip on a day it had the truck
/// to itself still carried less because of what happened a fortnight later.
///
/// The daily rate is also a constant: the tapered monthly figure divided by
/// days worked is 43000/28 exactly for any month under 28 days, which is why a
/// trip's share does not move when the caller changes the date range.
#[tokio::test]
async fn a_days_rental_is_split_only_between_that_days_trips() {
    use apex::db::revenue::TAQA_RENTAL_PER_DAY;
    use apex::db::stats_queries::get_taqa_stats;

    let pool = support::fresh_db("apex_taqa_day_share").await;
    sqlx::raw_sql(
        "INSERT INTO fee_mappings (company, terminal, drop_off_point, distance, fee)
         VALUES ('TAQA','Suez','Site',100.0,0);
         -- Ten working days. On the tenth the car runs TWICE; every other day once.
         INSERT INTO trips (company, terminal, drop_off_point, car_no_plate,
                            tank_capacity, date, receipt_no)
         SELECT 'TAQA','Suez','Site','DAY-1', 30000,
                to_char(DATE '2025-05-01' + (n || ' days')::interval, 'YYYY-MM-DD'),
                'D' || n
         FROM generate_series(0, 9) AS n;
         INSERT INTO trips (company, terminal, drop_off_point, car_no_plate,
                            tank_capacity, date, receipt_no)
         VALUES ('TAQA','Suez','Site','DAY-1', 30000, '2025-05-10', 'D9b');",
    )
    .execute(&pool)
    .await
    .unwrap();

    let filters = TripListFilters {
        page: 1,
        limit: MAX_LIMIT,
        company: Some("TAQA".into()),
        from: Some("2025-05-01".into()),
        to: Some("2025-05-31".into()),
        ..Default::default()
    }
    .normalized();
    let (rows, _) = list_trips(&pool, &filters, true).await.unwrap();
    assert_eq!(rows.len(), 11, "expected ten days, one of them doubled");

    let share = |receipt: &str| {
        rows.iter()
            .find(|r| r.receipt_no == receipt)
            .unwrap_or_else(|| panic!("{receipt} missing"))
            .allocated_rental
            .unwrap()
    };

    // A day the car had to itself: the whole day's rental, which is the flat
    // 43000/28 -- NOT reduced by what happened on the 10th.
    for receipt in ["D0", "D5", "D8"] {
        assert!(
            (share(receipt) - TAQA_RENTAL_PER_DAY).abs() < 0.01,
            "{receipt} carries {} but had the truck to itself",
            share(receipt)
        );
    }

    // The shared day: half each, and only these two are affected.
    let half = TAQA_RENTAL_PER_DAY / 2.0;
    for receipt in ["D9", "D9b"] {
        assert!(
            (share(receipt) - half).abs() < 0.01,
            "{receipt} carries {} but shared its day with one other trip",
            share(receipt)
        );
    }

    // And it still adds up to the month's rental.
    let stats_rental: f64 = get_taqa_stats(&pool, "2025-05-01", "2025-05-31", true)
        .await
        .unwrap()
        .iter()
        .filter_map(|d| d.car_rental)
        .sum();
    let allocated: f64 = rows.iter().filter_map(|r| r.allocated_rental).sum();
    assert!(
        (allocated - stats_rental).abs() < 0.01,
        "allocated {allocated} != statistics {stats_rental}"
    );
}
