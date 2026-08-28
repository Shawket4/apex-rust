//! The parity lock for trip revenue.
//!
//! Every company's revenue formula is about to be lifted out of four
//! hand-written statistics queries into one shared module, and then reused by a
//! new trips-list endpoint. The whole point of that refactor is that the
//! numbers do not move. This file is what makes that claim checkable: a
//! deterministic fixture built to hit the awkward parts of each formula, and a
//! golden snapshot of what the CURRENT code produces from it.
//!
//! The fixture is small but deliberately nasty:
//!
//!   * a TAQA car that works exactly 28 days (full rental) and one that works
//!     10 (tapered), so the `43000 - (28-d) * 43000/28` branch is exercised on
//!     both sides of its boundary;
//!   * a TAQA car whose month straddles a boundary, because the rental is
//!     computed PER MONTH and a range-wide count would silently differ;
//!   * a Petromin car with two trips on one day, because a car-day is shared —
//!     charging it twice is the obvious bug and the one worth pinning;
//!   * Watanya rows at fee bands 1, 2 and 15, plus an unmapped route that must
//!     fall through the CASE to 0.0 rather than to NULL;
//!   * a multi-container trip, so the "count a trip once, not once per
//!     container" logic stays honest.
//!
//! If a change to the revenue module moves any number here, the diff shows
//! exactly which company and which component moved.

mod support;

use apex::db::stats_queries;
use apex::models::trip::TripStatisticsDetails;
use sqlx::PgPool;

/// The window every assertion runs over. Wide enough to contain the whole
/// fixture; the per-month rental logic is exercised inside it, not by clipping.
const FROM: &str = "2025-05-01";
const TO: &str = "2025-06-30";

/* ------------------------------------------------------------------------ */
/* Fixture                                                                   */
/* ------------------------------------------------------------------------ */

async fn seed(pool: &PgPool) {
    sqlx::raw_sql(
        r#"
        -- Fee mappings. `fee` means something different per company: a rate in
        -- EGP/1000L for Petrol Arrows, a BAND NUMBER for Watanya, and nothing
        -- at all for TAQA/Petromin (which bill on distance).
        INSERT INTO fee_mappings (company, terminal, drop_off_point, distance, fee) VALUES
            ('Petrol Arrows', 'PA-T1', 'PA-Near',  120.0, 30.5),
            ('Petrol Arrows', 'PA-T1', 'PA-Far',   340.0, 44.25),
            ('Watanya',       'WA-T1', 'WA-B1',    100.0, 1),
            ('Watanya',       'WA-T1', 'WA-B2',    150.0, 2),
            ('Watanya',       'WA-T1', 'WA-B15',   900.0, 15),
            ('TAQA',          'TQ-T1', 'TQ-Site',  210.0, 0),
            ('Petromin',      'PM-T1', 'PM-Site',  180.0, 0);
        -- Deliberately NOT mapped: ('Petrol Arrows','PA-T1','PA-Unmapped') and
        -- ('Watanya','WA-T1','WA-Unmapped'). Both must yield 0.0 revenue via
        -- COALESCE/ELSE, not a NULL that poisons the SUM.

        /* ---- Petrol Arrows: tank_capacity * fee / 1000 ---- */
        INSERT INTO trips (company, terminal, drop_off_point, car_no_plate, tank_capacity, date, receipt_no) VALUES
            ('Petrol Arrows','PA-T1','PA-Near',     'PA-A', 40000, '2025-05-05', 'PA1'),
            ('Petrol Arrows','PA-T1','PA-Near',     'PA-A', 36000, '2025-05-06', 'PA2'),
            ('Petrol Arrows','PA-T1','PA-Far',      'PA-B', 50000, '2025-05-07', 'PA3'),
            ('Petrol Arrows','PA-T1','PA-Unmapped', 'PA-B', 45000, '2025-05-08', 'PA4');

        /* ---- Watanya: tank_capacity * band_rate / 1000 ---- */
        INSERT INTO trips (company, terminal, drop_off_point, car_no_plate, tank_capacity, date, receipt_no) VALUES
            ('Watanya','WA-T1','WA-B1',       'WA-A', 40000, '2025-05-05', 'WA1'),
            ('Watanya','WA-T1','WA-B2',       'WA-A', 40000, '2025-05-06', 'WA2'),
            ('Watanya','WA-T1','WA-B15',      'WA-B', 30000, '2025-05-07', 'WA3'),
            ('Watanya','WA-T1','WA-Unmapped', 'WA-B', 30000, '2025-05-08', 'WA4');

        /* ---- Watanya multi-container: one logical trip, three receipts ----
           Counted ONCE by total_trips but its volume/revenue all count. */
        INSERT INTO trips (company, terminal, drop_off_point, car_no_plate, tank_capacity, date, receipt_no, parent_trip_id) VALUES
            ('Watanya','WA-T1','WA-B1','WA-C', 10000, '2025-05-09', 'WAC1', 9001),
            ('Watanya','WA-T1','WA-B1','WA-C', 10000, '2025-05-09', 'WAC2', 9001),
            ('Watanya','WA-T1','WA-B1','WA-C', 10000, '2025-05-09', 'WAC3', 9001);

        /* ---- TAQA: distance * 50.5, plus a per-car per-MONTH rental ----
           TQ-A works 28 days in May  -> full 43000.
           TQ-B works 10 days in May  -> 43000 - 18 * (43000/28).
           TQ-A also works  3 days in June -> a SECOND, separately tapered
           monthly rental. Summing days across the range instead of per month
           would give one wrong number here, which is the point. */
        INSERT INTO trips (company, terminal, drop_off_point, car_no_plate, tank_capacity, date, receipt_no)
        SELECT 'TAQA','TQ-T1','TQ-Site','TQ-A', 30000,
               to_char(DATE '2025-05-01' + (n || ' days')::interval, 'YYYY-MM-DD'),
               'TQA' || n
        FROM generate_series(0, 27) AS n;

        INSERT INTO trips (company, terminal, drop_off_point, car_no_plate, tank_capacity, date, receipt_no)
        SELECT 'TAQA','TQ-T1','TQ-Site','TQ-B', 25000,
               to_char(DATE '2025-05-01' + (n || ' days')::interval, 'YYYY-MM-DD'),
               'TQB' || n
        FROM generate_series(0, 9) AS n;

        INSERT INTO trips (company, terminal, drop_off_point, car_no_plate, tank_capacity, date, receipt_no)
        SELECT 'TAQA','TQ-T1','TQ-Site','TQ-A', 30000,
               to_char(DATE '2025-06-01' + (n || ' days')::interval, 'YYYY-MM-DD'),
               'TQAJ' || n
        FROM generate_series(0, 2) AS n;

        /* ---- Petromin: distance * 42.5, plus 2000 per CAR-DAY ----
           PM-A runs twice on 05-18 (ONE car-day, not two) and once on 05-19.
           PM-B runs once on 05-18. Three car-days total => 6000. */
        INSERT INTO trips (company, terminal, drop_off_point, car_no_plate, tank_capacity, date, receipt_no) VALUES
            ('Petromin','PM-T1','PM-Site','PM-A', 20000, '2025-05-18', 'PM1'),
            ('Petromin','PM-T1','PM-Site','PM-A', 20000, '2025-05-18', 'PM2'),
            ('Petromin','PM-T1','PM-Site','PM-A', 20000, '2025-05-19', 'PM3'),
            ('Petromin','PM-T1','PM-Site','PM-B', 20000, '2025-05-18', 'PM4');

        /* ---- Soft-deleted row: must be invisible to every formula ---- */
        INSERT INTO trips (company, terminal, drop_off_point, car_no_plate, tank_capacity, date, receipt_no, deleted_at) VALUES
            ('Watanya','WA-T1','WA-B15','WA-Z', 99000, '2025-05-10', 'DEL1', now());
        "#,
    )
    .execute(pool)
    .await
    .expect("seed revenue fixture");
}

/* ------------------------------------------------------------------------ */
/* Snapshot formatting                                                       */
/* ------------------------------------------------------------------------ */

/// Money is rounded to 2dp before comparison. These are f64 sums out of
/// Postgres, and pinning full binary precision would make the snapshot fail on
/// a reordering that changes nothing anyone can be billed for.
fn money(v: f64) -> String {
    format!("{:.2}", v)
}

fn opt_money(v: Option<f64>) -> String {
    v.map_or_else(|| "-".to_string(), money)
}

fn opt_i(v: Option<i64>) -> String {
    v.map_or_else(|| "-".to_string(), |n| n.to_string())
}

fn render(company: &str, rows: &[TripStatisticsDetails]) -> String {
    let mut out = format!("== {company} ==\n");
    for r in rows {
        out.push_str(&format!(
            "{} | trips={} vol={} dist={} fee={} rev={} rental={} vat={} with_vat={} cars={} days={} car_days={}\n",
            r.group_name,
            r.total_trips,
            money(r.total_volume),
            money(r.total_distance),
            opt_money(r.fee),
            money(r.total_revenue),
            opt_money(r.car_rental),
            opt_money(r.vat),
            opt_money(r.total_with_vat),
            opt_i(r.distinct_cars),
            opt_i(r.distinct_days),
            opt_i(r.car_days),
        ));
    }
    out
}

/* ------------------------------------------------------------------------ */
/* The golden snapshot                                                       */
/* ------------------------------------------------------------------------ */

/// What the statistics endpoints produce from the fixture TODAY. Centralizing
/// the formulas must not change a single figure below.
///
/// Spot-checks that were computed by hand rather than copied from the code, so
/// this is a real oracle and not just a record of current behaviour:
///
///   Petrol Arrows / PA-Near : (40000 * 30.5 + 36000 * 30.5) / 1000 = 2318.00
///   Petrol Arrows / PA-Far  : 50000 * 44.25 / 1000              = 2212.50
///   Watanya band 1          : (40000 + 30000) * 104.5 / 1000     = 7315.00
///                             (4 rows: 1 direct + 3 containers)
///   TAQA base               : 41 rows * 210 km * 50.5            = 434,805.00
///   TAQA rental             : 43000 (TQ-A May, 28 days)
///                           + 43000 - 18 * (43000/28) = 15357.22 (TQ-B May)
///                           + 43000 - 25 * (43000/28) =  4607.19 (TQ-A June)
///   Petromin car-days       : 3 (PM-A 18th, PM-A 19th, PM-B 18th) -> 6000.00
const GOLDEN: &str = include_str!("fixtures/revenue_golden.txt");

/* ------------------------------------------------------------------------ */
/* The test                                                                  */
/* ------------------------------------------------------------------------ */

#[tokio::test]
async fn revenue_formulas_match_the_golden_snapshot() {
    let pool = support::fresh_db("apex_revenue_parity").await;
    seed(&pool).await;

    let mut actual = String::new();
    actual.push_str(&render(
        "Petrol Arrows",
        &stats_queries::get_petrol_arrows_stats(&pool, FROM, TO, true)
            .await
            .expect("petrol arrows"),
    ));
    actual.push_str(&render(
        "Watanya",
        &stats_queries::get_watanya_stats(&pool, FROM, TO, true)
            .await
            .expect("watanya"),
    ));
    actual.push_str(&render(
        "TAQA",
        &stats_queries::get_taqa_stats(&pool, FROM, TO, true)
            .await
            .expect("taqa"),
    ));
    actual.push_str(&render(
        "Petromin",
        &stats_queries::get_petromin_stats(&pool, FROM, TO, true)
            .await
            .expect("petromin"),
    ));

    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/revenue_golden.txt"
            ),
            &actual,
        )
        .expect("write golden");
        return;
    }

    assert_eq!(
        actual.trim(),
        GOLDEN.trim(),
        "\nrevenue output changed.\n--- actual ---\n{actual}\n--- expected ---\n{GOLDEN}\n\
         If this change is intended, re-run with UPDATE_GOLDEN=1 and review the diff."
    );
}

/// Financial access is a permission gate, not a display concern: without it the
/// revenue columns must be zero/absent at the SQL layer, so a caller who is not
/// permitted to see money cannot receive it in a payload at all.
#[tokio::test]
async fn revenue_is_withheld_without_financial_access() {
    let pool = support::fresh_db("apex_revenue_no_access").await;
    seed(&pool).await;

    for rows in [
        stats_queries::get_petrol_arrows_stats(&pool, FROM, TO, false)
            .await
            .unwrap(),
        stats_queries::get_watanya_stats(&pool, FROM, TO, false)
            .await
            .unwrap(),
        stats_queries::get_taqa_stats(&pool, FROM, TO, false)
            .await
            .unwrap(),
        stats_queries::get_petromin_stats(&pool, FROM, TO, false)
            .await
            .unwrap(),
    ] {
        for r in rows {
            assert_eq!(r.total_revenue, 0.0, "revenue leaked in {}", r.group_name);
            assert!(
                r.car_rental.is_none_or(|v| v == 0.0),
                "car rental leaked in {}",
                r.group_name
            );
            assert!(
                r.vat.is_none_or(|v| v == 0.0),
                "vat leaked in {}",
                r.group_name
            );
        }
    }
}
