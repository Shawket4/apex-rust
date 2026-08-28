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
/* A known quirk, pinned                                                     */
/* ------------------------------------------------------------------------ */

/// A TAQA car that serves two terminals on the same day is credited a working
/// day in each terminal's group — one day of work counted twice — so the fleet
/// rental comes out one day's taper high.
///
/// This is not something the allocation introduced. It is how the statistics
/// queries have always grouped their working-day counts, and it is live: car
/// "ف ع ص 4381" ran Alex and Suez on 2025-06-05, worth 1535.71. Counting the
/// day once is almost certainly the right reading of the rental contract, but
/// changing it changes what TAQA is billed, so the behaviour is pinned here
/// rather than quietly corrected.
///
/// What this test actually guards is that the list and statistics stay in
/// agreement about it. Whichever way the question is settled, they must move
/// together.
#[tokio::test]
async fn a_car_at_two_terminals_in_one_day_is_billed_twice_for_it() {
    use apex::db::revenue::TAQA_RENTAL_PER_DAY;
    use apex::db::stats_queries::get_taqa_stats;

    let pool = support::fresh_db("apex_trips_two_terminals").await;
    sqlx::raw_sql(
        "INSERT INTO fee_mappings (company, terminal, drop_off_point, distance, fee) VALUES
            ('TAQA','Alex','Site',100.0,0), ('TAQA','Suez','Site',100.0,0);
         -- One car, ten days at Alex. The tenth day it also runs from Suez.
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

    // The two must agree. That is the contract.
    assert!(
        (list_rental - stats_rental).abs() < 0.01,
        "list rental {list_rental} != statistics {stats_rental}"
    );

    // And they agree on the double-count: Alex sees 10 working days, Suez sees
    // 1, for 11 days of credit against 10 days of actual work.
    let alex = 43_000.0 - (28.0 - 10.0) * TAQA_RENTAL_PER_DAY;
    let suez = 43_000.0 - (28.0 - 1.0) * TAQA_RENTAL_PER_DAY;
    assert!(
        (stats_rental - (alex + suez)).abs() < 0.01,
        "expected {} (Alex {alex} + Suez {suez}), got {stats_rental}",
        alex + suez
    );

    // Stated the other way: counting 2025-05-10 once would bill exactly one
    // day's taper less. This is the figure a decision here would move.
    let counted_once = 43_000.0 - (28.0 - 10.0) * TAQA_RENTAL_PER_DAY;
    assert!(
        (stats_rental - counted_once - suez).abs() < 0.01,
        "the gap should be exactly Suez's separate taper"
    );
}
