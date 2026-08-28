//! Every trip revenue rule Apex has, in one place.
//!
//! These formulas used to live inline in four statistics queries in
//! `stats_queries.rs` and, separately, in about thirty places in FalconGo's
//! `Controllers/trip.go`. They had already drifted apart: Go tapered TAQA's car
//! rental by 1433.0/day while Rust used 1535.71, so the same fleet billed two
//! different amounts depending on which endpoint you asked. Everything below is
//! now the single definition, and `tests/revenue_parity.rs` pins the output.
//!
//! # Two kinds of revenue
//!
//! The distinction that shapes this whole module:
//!
//! * **Base revenue is per-row.** Every trip earns it on its own, from its own
//!   volume or distance. It can be attributed to a single trip honestly.
//! * **Car rental is not.** TAQA's is per car per calendar month, tapered by
//!   how many days that car worked; Petromin's is per car-day, shared by every
//!   trip that car ran that day. Neither belongs to any individual trip.
//!
//! So the trips list shows base revenue as fact and rental/VAT as an explicit
//! *allocation* (see [`allocation`]), while statistics report the aggregate
//! directly. Both are computed from the constants here, which is what keeps
//! them reconcilable.
//!
//! # Why SQL strings
//!
//! The formulas are joins and window functions over a large table; evaluating
//! them in Rust would mean pulling the whole fleet into memory. The functions
//! below emit SQL fragments instead. Every fragment is built from constants in
//! this file -- no caller-supplied value is ever interpolated -- so there is no
//! injection surface, only assembly.

use std::fmt::Write as _;

/* ------------------------------------------------------------------------ */
/* Rates                                                                     */
/* ------------------------------------------------------------------------ */

/// Volume-priced companies quote their fee per 1,000 litres, while
/// `trips.tank_capacity` is in litres. This is that divisor, named so the
/// magic 1000.0 stops appearing loose in the SQL.
pub const LITRES_PER_FEE_UNIT: f64 = 1000.0;

/// Egyptian VAT, applied to base revenue plus car rental.
pub const VAT_RATE: f64 = 0.14;

/// TAQA bills distance at a flat rate per kilometre.
pub const TAQA_RATE_PER_KM: f64 = 50.5;

/// Petromin bills distance at a flat rate per kilometre.
pub const PETROMIN_RATE_PER_KM: f64 = 42.5;

/// A TAQA car's full monthly rental, earned at [`TAQA_FULL_RENTAL_DAYS`] days.
pub const TAQA_MONTHLY_RENTAL: f64 = 43_000.0;

/// Days of work that earn a TAQA car its full monthly rental.
pub const TAQA_FULL_RENTAL_DAYS: f64 = 28.0;

/// Deducted per day short of [`TAQA_FULL_RENTAL_DAYS`].
///
/// This is `43000 / 28` — a clean pro-rata that reaches zero at zero days
/// worked. FalconGo used a hand-entered 1433.0, which is not derived from
/// anything and leaves a ~2,876 floor for a car that never moved; that was a
/// bug and it under-billed the taper. The value is computed, not typed, so the
/// two can never drift again.
pub const TAQA_RENTAL_PER_DAY: f64 = TAQA_MONTHLY_RENTAL / TAQA_FULL_RENTAL_DAYS;

/// Petromin rents by the car-day. Two trips by one car on one day are one day.
pub const PETROMIN_RENTAL_PER_CAR_DAY: f64 = 2_000.0;

/// Watanya prices by fee band, not by distance: the `fee_mappings.fee` column
/// holds a band NUMBER (1..15), and each band has a rate per 1,000 litres.
///
/// Held as data so the SQL `CASE` is generated rather than hand-written — the
/// previous hand-written copies were 15 lines each in three separate queries.
/// A route mapped to a band outside this table earns nothing, which is
/// deliberate: an unpriced band is a data-entry error, and inventing a rate for
/// it would quietly bill the customer for a guess.
pub const WATANYA_BAND_RATES: [(i32, f64); 15] = [
    (1, 104.5),
    (2, 122.1),
    (3, 129.8),
    (4, 156.2),
    (5, 183.7),
    (6, 196.9),
    (7, 210.1),
    (8, 235.4),
    (9, 261.8),
    (10, 288.2),
    (11, 314.6),
    (12, 341.0),
    (13, 367.4),
    (14, 393.8),
    (15, 420.2),
];

/* ------------------------------------------------------------------------ */
/* Companies                                                                 */
/* ------------------------------------------------------------------------ */

/// The four companies Apex hauls for. Each has its own revenue rule; there is
/// no default, because a company nobody has priced must not silently earn zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Company {
    PetrolArrows,
    Watanya,
    Taqa,
    Petromin,
}

impl Company {
    /// Exact match on the `trips.company` value. Returns `None` for anything
    /// unrecognised so callers must decide explicitly what to do about it.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "Petrol Arrows" => Some(Self::PetrolArrows),
            "Watanya" => Some(Self::Watanya),
            "TAQA" => Some(Self::Taqa),
            "Petromin" => Some(Self::Petromin),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PetrolArrows => "Petrol Arrows",
            Self::Watanya => "Watanya",
            Self::Taqa => "TAQA",
            Self::Petromin => "Petromin",
        }
    }

    /// Does this company rent its cars in addition to billing trips?
    pub fn has_car_rental(self) -> bool {
        matches!(self, Self::Taqa | Self::Petromin)
    }

    /// Does this company's invoice carry VAT?
    ///
    /// Petrol Arrows is the exception, and it is a real one rather than an
    /// oversight — its statistics have never carried a VAT column.
    pub fn has_vat(self) -> bool {
        !matches!(self, Self::PetrolArrows)
    }
}

/* ------------------------------------------------------------------------ */
/* Base revenue — per row                                                    */
/* ------------------------------------------------------------------------ */

/// SQL for a single trip's own revenue, as a `float8` expression.
///
/// `trips` and `fees` are the aliases the caller gave the `trips` table and its
/// `LEFT JOIN fee_mappings`. The join must be LEFT: an unmapped route yields
/// NULL, and every expression here coalesces that to 0.0 rather than letting it
/// poison a `SUM`.
///
/// Volume-priced companies divide by 1000 because `tank_capacity` is in litres
/// while the rate is per 1,000 litres.
pub fn base_revenue_sql(company: Company, trips: &str, fees: &str) -> String {
    match company {
        Company::PetrolArrows => format!(
            "({trips}.tank_capacity * COALESCE({fees}.fee::float8, 0.0) / {LITRES_PER_FEE_UNIT:?})::float8"
        ),
        Company::Watanya => format!(
            "({trips}.tank_capacity * {} / {LITRES_PER_FEE_UNIT:?})::float8",
            watanya_band_rate_sql(&format!("{fees}.fee"))
        ),
        Company::Taqa => format!(
            "(COALESCE({fees}.distance, 0.0) * {TAQA_RATE_PER_KM})::float8"
        ),
        Company::Petromin => format!(
            "(COALESCE({fees}.distance, 0.0) * {PETROMIN_RATE_PER_KM})::float8"
        ),
    }
}

/// A `CASE` mapping a Watanya fee band to its rate per 1,000 litres, generated
/// from [`WATANYA_BAND_RATES`]. Unknown or unmapped bands fall to `0.0`.
pub fn watanya_band_rate_sql(band_expr: &str) -> String {
    let mut sql = format!("CASE COALESCE({band_expr}::int, 0)");
    for (band, rate) in WATANYA_BAND_RATES {
        // `write!` to a String cannot fail; the `_ =` documents that.
        _ = write!(sql, " WHEN {band} THEN {rate:?}");
    }
    sql.push_str(" ELSE 0.0 END");
    sql
}

/* ------------------------------------------------------------------------ */
/* Car rental — per car, never per trip                                      */
/* ------------------------------------------------------------------------ */

/// TAQA's monthly rental for a car that worked `days_expr` days that month.
///
/// Full rental at 28 days or more, otherwise tapered by
/// [`TAQA_RENTAL_PER_DAY`] for each day short, floored at zero. A car with no
/// days earns nothing.
pub fn taqa_monthly_rental_sql(days_expr: &str) -> String {
    format!(
        "CASE \
           WHEN {days_expr} >= {TAQA_FULL_RENTAL_DAYS} THEN {TAQA_MONTHLY_RENTAL:?} \
           WHEN {days_expr} > 0 THEN GREATEST(0.0, {TAQA_MONTHLY_RENTAL:?} - \
             (({TAQA_FULL_RENTAL_DAYS} - {days_expr}) * {TAQA_RENTAL_PER_DAY:?})) \
           ELSE 0.0 \
         END::float8"
    )
}

/// Petromin's rental for `car_days_expr` distinct car-days.
pub fn petromin_rental_sql(car_days_expr: &str) -> String {
    format!("(COALESCE({car_days_expr}, 0) * {PETROMIN_RENTAL_PER_CAR_DAY:?})::float8")
}

/// VAT on a taxable base, as SQL.
pub fn vat_sql(taxable_expr: &str) -> String {
    format!("(({taxable_expr}) * {VAT_RATE:?})::float8")
}

/* ------------------------------------------------------------------------ */
/* Trip counting                                                             */
/* ------------------------------------------------------------------------ */

/// Counts LOGICAL trips over a set of rows: a multi-container trip is one trip
/// however many receipts it carries, and a standalone trip is one trip.
///
/// `parent_trip_id` is 0 rather than NULL for some standalone rows — GORM wrote
/// both over the years — so both spellings mean "not a container".
pub fn logical_trip_count_sql(parent_col: &str) -> String {
    format!(
        "COALESCE(COUNT(DISTINCT {parent_col}) \
           FILTER (WHERE {parent_col} IS NOT NULL AND {parent_col} != 0), 0) \
         + COALESCE(COUNT(*) \
           FILTER (WHERE {parent_col} IS NULL OR {parent_col} = 0), 0)"
    )
}

/* ------------------------------------------------------------------------ */
/* Allocation — spreading an aggregate across the rows that produced it       */
/* ------------------------------------------------------------------------ */

/// Rules for pushing car rental and VAT down onto individual trip rows, for the
/// trips list.
///
/// This is an allocation, not a measurement, and the distinction matters:
///
/// * Petromin's 2,000 for a car-day is split equally among that car's rows that
///   day, because the day is genuinely shared between them.
/// * TAQA's tapered monthly rental is split equally among that car's rows that
///   month, for the same reason at a coarser grain.
/// * VAT is 14% of each row's own base plus its allocated rental. VAT is
///   linear, so allocating the base and rental first and taxing after gives the
///   same total as taxing the aggregate.
///
/// The rental is computed over **the caller's filtered window**, not over all
/// time. That is what makes the list reconcile: summing the allocated column
/// over a filtered list equals the statistics total for identical filters. The
/// cost is that a given trip's allocated share moves when the date range moves
/// — it is a share of a window, and the UI labels it as one.
///
/// Rows are weighted equally rather than by base revenue on purpose: a trip to
/// an unmapped route earns no base revenue but still consumes the car for the
/// day, and revenue-weighting would hand its rental to its neighbours.
pub mod allocation {
    /// Window definition for TAQA: rental is per car per calendar month.
    pub const TAQA_PARTITION: &str = "car_no_plate, DATE_TRUNC('month', date::date)";

    /// Window definition for Petromin: rental is per car per day.
    pub const PETROMIN_PARTITION: &str = "car_no_plate, date";

    /// Divides `total_expr` equally across every row in `partition`.
    ///
    /// `COUNT(*)` over the same partition can never be zero for a row that is
    /// in it, so this cannot divide by zero.
    pub fn share_sql(total_expr: &str, partition: &str) -> String {
        format!("(({total_expr}) / COUNT(*) OVER (PARTITION BY {partition}))::float8")
    }
}

/* ------------------------------------------------------------------------ */
/* Tests                                                                     */
/* ------------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;

    /// The constant FalconGo got wrong. Pinned as an exact figure so a "tidy-up"
    /// that rounds it to 1535.71 is caught: that rounding leaves a car that
    /// never worked owing -0.12, which `GREATEST` hides, and a car that worked
    /// one day over-credited by a few piastres.
    #[test]
    fn taqa_taper_is_exact_pro_rata() {
        assert_eq!(TAQA_RENTAL_PER_DAY, 43_000.0 / 28.0);
        // A car that worked zero days owes exactly nothing.
        let at_zero = TAQA_MONTHLY_RENTAL - (TAQA_FULL_RENTAL_DAYS * TAQA_RENTAL_PER_DAY);
        assert!(at_zero.abs() < 1e-9, "taper must reach zero, got {at_zero}");
    }

    /// A full month must pay exactly the headline rental, not a rounded
    /// approximation of it.
    #[test]
    fn full_month_pays_the_headline_rental() {
        let days = 28.0_f64;
        let rental = if days >= TAQA_FULL_RENTAL_DAYS {
            TAQA_MONTHLY_RENTAL
        } else {
            TAQA_MONTHLY_RENTAL - ((TAQA_FULL_RENTAL_DAYS - days) * TAQA_RENTAL_PER_DAY)
        };
        assert_eq!(rental, 43_000.0);
    }

    #[test]
    fn watanya_bands_are_contiguous_1_to_15() {
        for (i, (band, rate)) in WATANYA_BAND_RATES.iter().enumerate() {
            assert_eq!(*band, i as i32 + 1, "bands must be 1..15 in order");
            assert!(*rate > 0.0, "band {band} has no rate");
        }
        // Rates rise with the band; a decrease would mean a longer haul paying
        // less, which is always a typo.
        for pair in WATANYA_BAND_RATES.windows(2) {
            assert!(
                pair[1].1 > pair[0].1,
                "band {} pays less than band {}",
                pair[1].0,
                pair[0].0
            );
        }
    }

    /// The generated CASE must cover every band and still have an ELSE, so an
    /// unmapped route yields 0.0 rather than NULL.
    #[test]
    fn generated_band_case_covers_every_band() {
        let sql = watanya_band_rate_sql("fm.fee");
        for (band, rate) in WATANYA_BAND_RATES {
            assert!(
                sql.contains(&format!("WHEN {band} THEN {rate:?}")),
                "band {band} missing from generated SQL"
            );
        }
        assert!(sql.contains("ELSE 0.0 END"));
        assert!(sql.contains("COALESCE(fm.fee::int, 0)"));
    }

    /// Each company's base revenue must reference the columns it actually bills
    /// on — volume for the fee-priced pair, distance for the per-km pair. A
    /// swap here would be invisible in review and wrong in production.
    #[test]
    fn base_revenue_bills_on_the_right_column() {
        let pa = base_revenue_sql(Company::PetrolArrows, "t", "fm");
        assert!(pa.contains("t.tank_capacity") && pa.contains("fm.fee"));
        assert!(!pa.contains("distance"));

        let wa = base_revenue_sql(Company::Watanya, "t", "fm");
        assert!(wa.contains("t.tank_capacity") && wa.contains("WHEN 15 THEN 420.2"));
        assert!(!wa.contains("distance"));

        let tq = base_revenue_sql(Company::Taqa, "t", "fm");
        assert!(tq.contains("fm.distance") && tq.contains("50.5"));
        assert!(!tq.contains("tank_capacity"));

        let pm = base_revenue_sql(Company::Petromin, "t", "fm");
        assert!(pm.contains("fm.distance") && pm.contains("42.5"));
        assert!(!pm.contains("tank_capacity"));
    }

    #[test]
    fn company_names_round_trip() {
        for c in [
            Company::PetrolArrows,
            Company::Watanya,
            Company::Taqa,
            Company::Petromin,
        ] {
            assert_eq!(Company::from_name(c.as_str()), Some(c));
        }
        assert_eq!(Company::from_name("Petrolarrows"), None);
        assert_eq!(Company::from_name(""), None);
    }

    /// Only TAQA and Petromin rent cars; only Petrol Arrows is VAT-free. These
    /// pair up in a way that is easy to conflate.
    #[test]
    fn rental_and_vat_apply_to_the_right_companies() {
        assert!(Company::Taqa.has_car_rental() && Company::Petromin.has_car_rental());
        assert!(!Company::PetrolArrows.has_car_rental() && !Company::Watanya.has_car_rental());

        assert!(!Company::PetrolArrows.has_vat());
        assert!(Company::Watanya.has_vat() && Company::Taqa.has_vat() && Company::Petromin.has_vat());
    }
}
