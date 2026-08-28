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
use sqlx::Row;

/// The window every assertion runs over. Wide enough to contain the whole
/// fixture; the per-month rental logic is exercised inside it, not by clipping.
const FROM: &str = "2025-05-01";
const TO: &str = "2025-06-30";

/* ------------------------------------------------------------------------ */
/* Fixture                                                                   */
/* ------------------------------------------------------------------------ */

/// The fixture lives in `support` because the trips-list suite asserts against
/// the very same rows — that is the point of it, since the two must agree.
use support::seed_trip_fixture as seed;


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

/// What the statistics endpoints produce from the fixture. Centralizing the
/// formulas must not change a single figure below.
///
/// One figure DID move when the formulas were centralized, deliberately and
/// exactly once: TAQA's taper was a typed 1535.71, and is now the exact
/// 43000/28 it was always meant to approximate. That shifted TAQA's rental by
/// -0.18 on this fixture (62964.47 -> 62964.29) and nothing else, in any
/// company or column -- which is the evidence that the refactor itself was
/// behaviour-preserving. The typed value also never reached zero: a car that
/// worked no days was credited 43000 - 28 * 1535.71 = 0.12 it had not earned.
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
///                           + 43000 - 18 * (43000/28) = 15357.14 (TQ-B May)
///                           + 43000 - 25 * (43000/28) =  4607.14 (TQ-A June)
///                           = 62964.29
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

/* ------------------------------------------------------------------------ */
/* Reconciliation                                                            */
/*                                                                           */
/* The property the whole allocation design rests on.                        */
/* ------------------------------------------------------------------------ */

/// Summing the per-row revenue over a window must equal what the statistics
/// endpoints report for the same window — to the cent, for every component.
///
/// This is the claim that makes a per-trip revenue column defensible at all.
/// Car rental is not a per-trip quantity: TAQA's is earned per car per month
/// and Petromin's per car-day, shared by every trip that car ran. The list
/// divides those costs across the rows that incurred them, and this test is
/// what proves the division neither invents money nor loses it — that the
/// column a user sees on the trips page adds up to the number the statistics
/// page shows them.
///
/// It is also the regression that catches the tempting mistakes: charging a
/// Petromin car twice for two trips on one day, collapsing TAQA's separate
/// monthly rentals into one range-wide taper, or dropping the rows of an
/// unmapped route that earn no base revenue but still consume a car-day.
#[tokio::test]
async fn per_row_revenue_sums_to_the_statistics_aggregate() {
    use apex::db::revenue::allocation::per_row_revenue_cte;

    let pool = support::fresh_db("apex_revenue_reconcile").await;
    seed(&pool).await;

    for (company, stats) in [
        (
            "Petrol Arrows",
            stats_queries::get_petrol_arrows_stats(&pool, FROM, TO, true)
                .await
                .unwrap(),
        ),
        (
            "Watanya",
            stats_queries::get_watanya_stats(&pool, FROM, TO, true)
                .await
                .unwrap(),
        ),
        (
            "TAQA",
            stats_queries::get_taqa_stats(&pool, FROM, TO, true)
                .await
                .unwrap(),
        ),
        (
            "Petromin",
            stats_queries::get_petromin_stats(&pool, FROM, TO, true)
                .await
                .unwrap(),
        ),
    ] {
        // The window is the same one statistics aggregates over: this company,
        // this date range. Nothing else — see per_row_revenue_cte's contract.
        let sql = format!(
            "WITH {} SELECT \
               COALESCE(SUM(base_revenue), 0.0)::float8     AS base, \
               COALESCE(SUM(allocated_rental), 0.0)::float8 AS rental, \
               COALESCE(SUM(allocated_vat), 0.0)::float8    AS vat, \
               COALESCE(SUM(allocated_total), 0.0)::float8  AS total \
             FROM revenue",
            per_row_revenue_cte("t.company = $1 AND t.date BETWEEN $2 AND $3")
        );
        let row = sqlx::query(&sql)
            .bind(company)
            .bind(FROM)
            .bind(TO)
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|e| panic!("per-row revenue for {company}: {e}"));

        let (row_base, row_rental, row_vat, row_total) = (
            row.get::<f64, _>("base"),
            row.get::<f64, _>("rental"),
            row.get::<f64, _>("vat"),
            row.get::<f64, _>("total"),
        );

        // Statistics report per group (drop-off point, fee band or terminal);
        // the list has no grouping, so compare against the company total.
        let stat_base: f64 = stats.iter().map(|d| d.total_revenue).sum();
        let stat_rental: f64 = stats.iter().filter_map(|d| d.car_rental).sum();
        let stat_vat: f64 = stats.iter().filter_map(|d| d.vat).sum();

        // A cent. These are f64 sums taken in different orders, so exact
        // equality would be testing float associativity rather than the rule.
        const EPS: f64 = 0.01;
        let close = |a: f64, b: f64| (a - b).abs() < EPS;

        assert!(
            close(row_base, stat_base),
            "{company}: base revenue {row_base} != statistics {stat_base}"
        );
        assert!(
            close(row_rental, stat_rental),
            "{company}: allocated rental {row_rental} != statistics {stat_rental}"
        );
        assert!(
            close(row_vat, stat_vat),
            "{company}: allocated VAT {row_vat} != statistics {stat_vat}"
        );
        assert!(
            close(row_total, stat_base + stat_rental + stat_vat),
            "{company}: allocated total {row_total} != {}",
            stat_base + stat_rental + stat_vat
        );
    }
}
