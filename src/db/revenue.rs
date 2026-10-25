//! Where trip revenue comes from: FalconGo prices it, apex-rust reads it.
//!
//! This module used to be the definition of revenue — the rates, the Watanya
//! band table, TAQA's taper, Petromin's car-day, emitted as SQL and evaluated
//! on every read. That definition now lives in FalconGo's database as
//! effective-dated rules (`pricing_rules`, `pricing_bands`), and FalconGo's
//! pricing engine applies them **once, on write**, storing the result. apex-rust
//! never computes a figure; it reads these, and nothing else:
//!
//! * **`trips.base_revenue`, `trips.revenue_vat`** — a trip's own revenue
//!   before VAT, and the VAT on it, under the rule in force on its date. NULL
//!   when the trip could not be priced, with the reason in
//!   `trips.pricing_error` (no fee mapping, a band with no rate, a company
//!   with no rule, an unregistered drop-off). Read as 0 in sums, and counted
//!   on the dashboard as a trip earning nothing.
//! * **`truck_month_charges`** — car rental and its VAT, one row per car,
//!   company, terminal and calendar month. TAQA rents a car by the month,
//!   tapered by the days it worked; Petromin by the car-day. Rental belongs to
//!   the car-month, never to a trip.
//! * **the `trip_revenue` view** — each row's base and VAT plus its
//!   *allocated* share of its car-month's rental (see below). Every per-row
//!   and every aggregate revenue figure apex-rust serves is a sum over this
//!   view, which is what keeps the trips list, the statistics page, the route
//!   days panel, the dashboard and the advisor reconciled by construction.
//!
//! # Rental over a date range
//!
//! A range that is not whole months gets a share of each month's stored
//! rental. The rule (defined by FalconGo's view, followed by everything here):
//!
//! > A car-month's stored rental is spread evenly over the days the car worked
//! > that month — `rental / month_days_worked` per worked day — and a day's
//! > share is split equally between the car's trips that day. A range earns
//! > the shares of the worked days that fall inside it.
//!
//! So a range covering a whole month earns exactly that month's stored rental
//! (the days sum back to it), a range covering half a car's worked days earns
//! half, and a trip's own share never moves with the caller's range — it is
//! the month's figure, not a figure recomputed over whatever window was asked
//! for. Statistics' per-terminal rental is the sum of those shares over the
//! terminal's rows. It equals the stored per-terminal `truck_month_charges`
//! row for a whole month except on a date a car served two terminals, where
//! the day is split by trips rather than by terminals; the car's total is
//! exact either way.
//!
//! # Who may see it
//!
//! Every money figure is withheld below permission 3 (statistics, route days)
//! and below 4 on the per-trip list and the dashboard, which put a figure
//! against one driver's trip. A fee mapping's `fee` is a rate for Petrol
//! Arrows (EGP per 1,000 L), so it is withheld too — see [`fee_visible`].

/// The permission at which aggregate revenue becomes visible.
pub const FINANCIAL_PERMISSION: i32 = 3;

/// Whether a fee mapping's raw `fee` may be shown at this access level.
///
/// For Petrol Arrows it is a price (EGP per 1,000 L), so it is financial data.
/// For Watanya it is a band NUMBER (1..15) — a route category, already visible
/// in the statistics' group names, and the key the statistics page uses to
/// open a band's routes — so it stays. TAQA and Petromin have no fee.
pub fn fee_visible(company: &str, financial: bool) -> bool {
    financial || company == "Watanya"
}

/// The per-km rate in force for `company` on `date` (YYYY-MM-DD), from the
/// pricing rules. `None` for a company priced by volume, or with no rule.
pub async fn per_km_rate_on(
    pool: &sqlx::PgPool,
    company: &str,
    date: &str,
) -> anyhow::Result<Option<f64>> {
    let rate: Option<Option<f64>> = sqlx::query_scalar(
        "SELECT per_km_rate::float8 FROM pricing_rules \
          WHERE company = $1 AND to_char(effective_from, 'YYYY-MM-DD') <= $2 \
          ORDER BY effective_from DESC LIMIT 1",
    )
    .bind(company)
    .bind(date)
    .fetch_optional(pool)
    .await?;
    Ok(rate.flatten())
}

/// The per-row revenue chain every surface reads.
pub mod allocation {
    use crate::db::trip_shape::trip_distance_sql;

    /// The CTE chain that hangs the stored revenue breakdown off every trip in
    /// a window. It ends in a CTE named `revenue`, which yields `t.*` (every
    /// trips column, including the stored `base_revenue` NUMERIC and
    /// `pricing_error`) plus, as float8:
    ///
    /// * `fee_distance`, `fee_value` — the fee mapping's distance and raw fee;
    /// * `trip_distance` — the logical trip's distance on its furthest row;
    /// * `revenue_base` — stored base revenue, 0 when unpriced;
    /// * `allocated_rental`, `allocated_vat`, `allocated_total` — from the
    ///   `trip_revenue` view.
    ///
    /// `window_sql` is a boolean predicate over `t` using only `t.company` and
    /// `t.date`. It is applied to the view as well as to trips, because those
    /// are the view's partition columns: Postgres pushes the predicate below
    /// the view's window function instead of allocating the whole table. It
    /// must not carry a search or status filter — not because it would change
    /// the money (the allocation is the month's, whatever the window), but
    /// because `trip_distance` attributes a trip's distance within the window.
    pub fn per_row_revenue_cte(window_sql: &str) -> String {
        format!(
            r#"
        priced AS (
            SELECT t.trip_id,
                   t.base_revenue::float8     AS stored_base,
                   t.allocated_rental::float8 AS allocated_rental,
                   t.allocated_vat::float8    AS allocated_vat,
                   t.allocated_total::float8  AS allocated_total
              FROM trip_revenue t
             WHERE {window_sql}
        ),
        revenue AS (
            SELECT
                t.*,
                COALESCE(fm.distance, 0.0)::float8 AS fee_distance,
                {trip_distance} AS trip_distance,
                COALESCE(fm.fee::float8, 0.0)      AS fee_value,
                COALESCE(p.stored_base, 0.0)::float8 AS revenue_base,
                p.allocated_rental,
                p.allocated_vat,
                p.allocated_total
            FROM trips t
            JOIN priced p ON p.trip_id = t.id
            LEFT JOIN fee_mappings fm
                ON  fm.company        = t.company
                AND fm.terminal       = t.terminal
                AND fm.drop_off_point = t.drop_off_point
                AND fm.deleted_at IS NULL
            WHERE t.deleted_at IS NULL AND ({window_sql})
        )"#,
            trip_distance = trip_distance_sql("t", "fm"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chain_reads_the_view_and_computes_nothing() {
        let sql = allocation::per_row_revenue_cte("t.date BETWEEN $1 AND $2");
        assert!(sql.contains("FROM trip_revenue t"));
        // No rate, band or taper survives anywhere in the read path.
        for formula in ["50.5", "42.5", "104.5", "43000", "0.14", "/ 1000", "CASE t.company"] {
            assert!(!sql.contains(formula), "a revenue formula crept back in: {formula}");
        }
        // The window reaches the view as well as trips (pushdown).
        assert_eq!(sql.matches("t.date BETWEEN $1 AND $2").count(), 2);
    }

    #[test]
    fn a_petrol_arrows_rate_is_money_but_a_watanya_band_is_not() {
        assert!(!fee_visible("Petrol Arrows", false));
        assert!(fee_visible("Petrol Arrows", true));
        assert!(fee_visible("Watanya", false));
        assert!(!fee_visible("TAQA", false));
    }
}
