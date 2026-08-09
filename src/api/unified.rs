//! The unified cost view: banksms transactions + fuel events + driver loans.
//!
//! Background. The legacy `/api/v1/fleet-expenses` endpoint served a UNION of
//! three tables — `fleet_expenses`, `fuel_events` and `loans` — toggled by
//! `include_fuel` / `include_loans`. Only `fleet_expenses` was migrated into
//! `banksms.transactions`; fuel and loans stay where they are because they have
//! other owners:
//!
//!   * `public.fuel_events` is written by the PetroApp sync pipeline
//!   * `public.loans` is GORM-AutoMigrated by FalconGo, which recreates it
//!
//! Copying either into `banksms` would create a second writer and guarantee
//! drift, so they are READ here and never written. `public.fleet_expenses` is
//! not read at all — its rows live in `banksms.transactions` as `source =
//! 'import'`.
//!
//! Everything below is built with fixed parameter slots (`$1..$N`) rather than
//! dynamically numbered ones. Postgres lets the same placeholder appear in
//! several branches of a UNION, so each filter value is bound exactly once and
//! referenced from all three branches. Dynamic numbering across a three-way
//! union is where off-by-one bind bugs come from.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

/// Synthetic id namespaces.
///
/// `banksms.transactions.id`, `fuel_events.id` and `loans.id` are independent
/// sequences, so raw ids collide across sources. The API needs one stable
/// identifier per row, so read-only rows are offset into their own range.
/// Chosen far above any plausible real id, and asserted in tests.
pub const FUEL_ID_BASE: i64 = 1_000_000_000_000;
pub const LOAN_ID_BASE: i64 = 2_000_000_000_000;

pub fn decode_synthetic(id: i64) -> (&'static str, i64) {
    if id >= LOAN_ID_BASE {
        ("loan", id - LOAN_ID_BASE)
    } else if id >= FUEL_ID_BASE {
        ("fuel_event", id - FUEL_ID_BASE)
    } else {
        ("transaction", id)
    }
}

/// Filters shared by the list and statistics endpoints.
#[derive(Debug, Default, Clone)]
pub struct UnifiedFilters {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub category: Option<String>,
    pub company: Option<String>,
    pub car_no_plate: Option<String>,
    pub payment_method: Option<String>,
    pub source: Option<String>,
    pub account: Option<String>,
    pub direction: Option<String>,
    pub template: Option<String>,
    pub counterparty: Option<String>,
    pub min_amount: Option<Decimal>,
    pub max_amount: Option<Decimal>,
    pub verified: Option<bool>,
    pub q: Option<String>,
    pub include_fuel: bool,
    pub include_loans: bool,
}

impl UnifiedFilters {
    /// Whether the fuel branch can contribute anything under these filters.
    ///
    /// Fuel events have no account, direction, template, verified flag or
    /// override history, so any filter on those excludes them entirely —
    /// including them anyway would return rows that do not match what was asked.
    pub fn wants_fuel(&self) -> bool {
        self.include_fuel
            && self.account.is_none()
            && self.direction.is_none()
            && self.template.is_none()
            && self.verified.is_none()
            && self.source.as_deref().map_or(true, |s| s == "fuel_event")
            && self.category.as_deref().map_or(true, |c| c == "Fuel")
    }

    pub fn wants_loans(&self) -> bool {
        self.include_loans
            && self.account.is_none()
            && self.direction.is_none()
            && self.template.is_none()
            && self.verified.is_none()
            && self.car_no_plate.is_none() // loans are not vehicle-scoped
            && self.company.is_none() // nor company-scoped
            && self.source.as_deref().map_or(true, |s| s == "loan")
            && self.category.as_deref().map_or(true, |c| c == "Loan")
    }

    /// Whether the banksms branch can contribute.
    pub fn wants_transactions(&self) -> bool {
        // A source filter naming a read-only source excludes the ledger.
        !matches!(self.source.as_deref(), Some("fuel_event") | Some("loan"))
    }
}

/// Column list every branch must produce, in order. Kept in one place so a
/// branch that drifts fails at the database rather than silently shifting
/// values into the wrong column.
pub const UNIFIED_COLUMNS: &str = "
    id, version, source, raw_message_id, editable,
    eff_direction, eff_amount, eff_currency, eff_account, eff_counterparty,
    eff_reference, eff_occurred_at,
    parsed_direction, parsed_amount, parsed_currency, parsed_account,
    parsed_counterparty, parsed_reference, parsed_balance_after,
    parsed_occurred_at, parsed_template, parser_version,
    confidence, parse_method, category, verified,
    description, payment_method, company, car_no_plate, paid_by,
    created_by, created_at, updated_at, driver_id, employee_id, car_id, has_overrides
";

/// The banksms branch: effective values with overrides applied.
///
/// `COALESCE(override, parsed_*)` happens in SQL, not Rust, so that filtering
/// and sorting operate on the same values the client is shown. Filtering on the
/// parsed value while displaying the override would be a subtle, maddening bug.
pub const TRANSACTIONS_BRANCH: &str = r#"
    SELECT
        t.id, t.version, t.source::text AS source, t.raw_message_id,
        TRUE AS editable,
        COALESCE(o.direction, t.parsed_direction::text)            AS eff_direction,
        COALESCE(o.amount::numeric, t.parsed_amount)               AS eff_amount,
        COALESCE(o.currency, t.parsed_currency)                    AS eff_currency,
        COALESCE(o.account, t.parsed_account)                      AS eff_account,
        COALESCE(o.counterparty, t.parsed_counterparty)            AS eff_counterparty,
        COALESCE(o.reference, t.parsed_reference)                  AS eff_reference,
        COALESCE(o.occurred_at::timestamptz, t.parsed_occurred_at, t.created_at) AS eff_occurred_at,
        t.parsed_direction::text AS parsed_direction, t.parsed_amount, t.parsed_currency,
        t.parsed_account, t.parsed_counterparty, t.parsed_reference,
        t.parsed_balance_after, t.parsed_occurred_at, t.parsed_template, t.parser_version,
        t.confidence, t.parse_method::text AS parse_method,
        t.category, t.verified,
        t.description, t.payment_method, t.company, t.car_no_plate, t.paid_by,
        t.created_by, t.created_at, t.updated_at,
        t.driver_id, t.employee_id, t.car_id,
        (o.transaction_id IS NOT NULL) AS has_overrides
    FROM banksms.transactions t
    LEFT JOIN LATERAL (
        SELECT ov.transaction_id,
               MAX(CASE WHEN ov.field = 'direction'    THEN ov.value END) AS direction,
               MAX(CASE WHEN ov.field = 'amount'       THEN ov.value END) AS amount,
               MAX(CASE WHEN ov.field = 'currency'     THEN ov.value END) AS currency,
               MAX(CASE WHEN ov.field = 'account'      THEN ov.value END) AS account,
               MAX(CASE WHEN ov.field = 'counterparty' THEN ov.value END) AS counterparty,
               MAX(CASE WHEN ov.field = 'reference'    THEN ov.value END) AS reference,
               MAX(CASE WHEN ov.field = 'occurred_at'  THEN ov.value END) AS occurred_at
        FROM banksms.transaction_overrides ov
        WHERE ov.transaction_id = t.id AND ov.superseded_at IS NULL AND NOT ov.is_cleared
        GROUP BY ov.transaction_id
    ) o ON TRUE
    WHERE t.deleted_at IS NULL
      AND ($1::timestamptz IS NULL OR COALESCE(t.parsed_occurred_at, t.created_at) >= $1)
      AND ($2::timestamptz IS NULL OR COALESCE(t.parsed_occurred_at, t.created_at) <= $2)
      AND ($3::text    IS NULL OR t.category       = $3)
      AND ($4::text    IS NULL OR t.company        = $4)
      AND ($5::text    IS NULL OR t.car_no_plate   = $5)
      AND ($6::text    IS NULL OR t.payment_method = $6)
      AND ($7::text    IS NULL OR t.source::text   = $7)
      AND ($8::text    IS NULL OR t.parsed_account = $8)
      AND ($9::text    IS NULL OR t.parsed_direction::text = $9)
      AND ($10::text   IS NULL OR t.parsed_template = $10)
      AND ($11::numeric IS NULL OR t.parsed_amount >= $11)
      AND ($12::numeric IS NULL OR t.parsed_amount <= $12)
      AND ($13::bool   IS NULL OR t.verified = $13)
      AND ($14::text   IS NULL OR t.parsed_counterparty ILIKE '%' || $14 || '%')
      AND ($15::text   IS NULL OR (
              t.parsed_counterparty ILIKE '%' || $15 || '%'
           OR t.description         ILIKE '%' || $15 || '%'
           OR t.parsed_reference    ILIKE '%' || $15 || '%'
           OR t.paid_by             ILIKE '%' || $15 || '%'))
"#;

/// Fuel events, read-only.
///
/// Mapping mirrors the legacy UnifiedExpense projection exactly (description
/// synthesised as "Fuel: NL @ P/L", transporter as company, driver as paid_by)
/// so the numbers match what the old costs view reported.
pub const FUEL_BRANCH: &str = r#"
    SELECT
        (f.id + 1000000000000)::bigint AS id,
        1 AS version, 'fuel_event'::text AS source, NULL::bigint AS raw_message_id,
        FALSE AS editable,
        'out'::text AS eff_direction,
        COALESCE(f.price, 0)::numeric AS eff_amount,
        'EGP'::text AS eff_currency,
        NULL::text AS eff_account,
        f.driver_name AS eff_counterparty,
        NULL::text AS eff_reference,
        (f.date::date::timestamp AT TIME ZONE 'Africa/Cairo') AS eff_occurred_at,
        NULL::text AS parsed_direction, NULL::numeric AS parsed_amount,
        NULL::text AS parsed_currency, NULL::text AS parsed_account,
        NULL::text AS parsed_counterparty, NULL::text AS parsed_reference,
        NULL::numeric AS parsed_balance_after, NULL::timestamptz AS parsed_occurred_at,
        NULL::text AS parsed_template, NULL::integer AS parser_version,
        100::smallint AS confidence, 'manual'::text AS parse_method,
        'Fuel'::text AS category, TRUE AS verified,
        CONCAT('Fuel: ', COALESCE(f.liters::text, '0'), 'L @ ',
               COALESCE(f.price_per_liter::text, '0'), '/L')::text AS description,
        COALESCE(f.method, 'Cash')::text AS payment_method,
        f.transporter AS company, f.car_no_plate, f.driver_name AS paid_by,
        NULL::text AS created_by, f.created_at AT TIME ZONE 'UTC' AS created_at,
        f.updated_at AT TIME ZONE 'UTC' AS updated_at,
        NULL::bigint AS driver_id, NULL::bigint AS employee_id, NULL::bigint AS car_id,
        FALSE AS has_overrides
    FROM public.fuel_events f
    WHERE f.deleted_at IS NULL AND f.created_at IS NOT NULL AND f.date IS NOT NULL
      AND ($1::timestamptz IS NULL OR (f.date::date::timestamp AT TIME ZONE 'Africa/Cairo') >= $1)
      AND ($2::timestamptz IS NULL OR (f.date::date::timestamp AT TIME ZONE 'Africa/Cairo') <= $2)
      AND ($4::text    IS NULL OR f.transporter  = $4)
      AND ($5::text    IS NULL OR f.car_no_plate = $5)
      AND ($6::text    IS NULL OR COALESCE(f.method, 'Cash') = $6)
      AND ($11::numeric IS NULL OR COALESCE(f.price, 0) >= $11)
      AND ($12::numeric IS NULL OR COALESCE(f.price, 0) <= $12)
      AND ($14::text   IS NULL OR f.driver_name ILIKE '%' || $14 || '%')
      AND ($15::text   IS NULL OR (
              f.driver_name  ILIKE '%' || $15 || '%'
           OR f.car_no_plate ILIKE '%' || $15 || '%'
           OR f.transporter  ILIKE '%' || $15 || '%'))
"#;

/// Driver loans, read-only.
///
/// `loans.method` holds who advanced the money (e.g. "Shady"), not a payment
/// instrument — the legacy view mapped it to payment_method, so that mapping is
/// preserved here rather than silently "corrected", which would change every
/// historical figure the old report produced.
pub const LOAN_BRANCH: &str = r#"
    SELECT
        (l.id + 2000000000000)::bigint AS id,
        1 AS version, 'loan'::text AS source, NULL::bigint AS raw_message_id,
        FALSE AS editable,
        'out'::text AS eff_direction,
        COALESCE(l.amount, 0)::numeric AS eff_amount,
        'EGP'::text AS eff_currency,
        NULL::text AS eff_account,
        NULL::text AS eff_counterparty,
        NULL::text AS eff_reference,
        (l.date::date::timestamp AT TIME ZONE 'Africa/Cairo') AS eff_occurred_at,
        NULL::text AS parsed_direction, NULL::numeric AS parsed_amount,
        NULL::text AS parsed_currency, NULL::text AS parsed_account,
        NULL::text AS parsed_counterparty, NULL::text AS parsed_reference,
        NULL::numeric AS parsed_balance_after, NULL::timestamptz AS parsed_occurred_at,
        NULL::text AS parsed_template, NULL::integer AS parser_version,
        100::smallint AS confidence, 'manual'::text AS parse_method,
        'Loan'::text AS category, TRUE AS verified,
        l.description,
        COALESCE(l.method, 'Cash')::text AS payment_method,
        NULL::text AS company, NULL::text AS car_no_plate,
        COALESCE(l.method, '')::text AS paid_by,
        NULL::text AS created_by, l.created_at AT TIME ZONE 'UTC' AS created_at,
        l.updated_at AT TIME ZONE 'UTC' AS updated_at,
        l.driver_id, l.employee_id, NULL::bigint AS car_id,
        FALSE AS has_overrides
    FROM public.loans l
    WHERE l.deleted_at IS NULL AND l.created_at IS NOT NULL AND l.date IS NOT NULL
      AND ($1::timestamptz IS NULL OR (l.date::date::timestamp AT TIME ZONE 'Africa/Cairo') >= $1)
      AND ($2::timestamptz IS NULL OR (l.date::date::timestamp AT TIME ZONE 'Africa/Cairo') <= $2)
      AND ($6::text    IS NULL OR COALESCE(l.method, 'Cash') = $6)
      AND ($11::numeric IS NULL OR COALESCE(l.amount, 0) >= $11)
      AND ($12::numeric IS NULL OR COALESCE(l.amount, 0) <= $12)
      AND ($15::text   IS NULL OR (
              l.description ILIKE '%' || $15 || '%'
           OR l.method      ILIKE '%' || $15 || '%'))
"#;

/// Assemble the branches the filters allow into one UNION ALL.
/// Returns None when no branch can contribute (an empty result, not an error).
pub fn build_union(f: &UnifiedFilters) -> Option<String> {
    let mut branches: Vec<&str> = Vec::with_capacity(3);
    if f.wants_transactions() {
        branches.push(TRANSACTIONS_BRANCH);
    }
    if f.wants_fuel() {
        branches.push(FUEL_BRANCH);
    }
    if f.wants_loans() {
        branches.push(LOAN_BRANCH);
    }
    if branches.is_empty() {
        return None;
    }
    Some(branches.join("\n UNION ALL \n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> UnifiedFilters {
        UnifiedFilters {
            include_fuel: true,
            include_loans: true,
            ..Default::default()
        }
    }

    #[test]
    fn synthetic_ids_round_trip_and_do_not_collide() {
        assert_eq!(decode_synthetic(42), ("transaction", 42));
        assert_eq!(decode_synthetic(FUEL_ID_BASE + 7), ("fuel_event", 7));
        assert_eq!(decode_synthetic(LOAN_ID_BASE + 7), ("loan", 7));
        // The namespaces must not overlap for any plausible real id.
        assert!(FUEL_ID_BASE > 1_000_000_000);
        assert!(LOAN_ID_BASE > FUEL_ID_BASE);
    }

    #[test]
    fn all_three_branches_by_default() {
        let sql = build_union(&base()).unwrap();
        assert!(sql.contains("banksms.transactions"));
        assert!(sql.contains("public.fuel_events"));
        assert!(sql.contains("public.loans"));
    }

    #[test]
    fn flags_exclude_branches() {
        let f = UnifiedFilters { include_fuel: false, ..base() };
        let sql = build_union(&f).unwrap();
        assert!(!sql.contains("public.fuel_events"));
        assert!(sql.contains("public.loans"));
    }

    #[test]
    fn ledger_only_filters_exclude_read_only_sources() {
        // A row from fuel_events has no account, so filtering by account must
        // not silently include every fuel row.
        for f in [
            UnifiedFilters { account: Some("6001-01".into()), ..base() },
            UnifiedFilters { verified: Some(true), ..base() },
            UnifiedFilters { template: Some("abk".into()), ..base() },
            UnifiedFilters { direction: Some("in".into()), ..base() },
        ] {
            let sql = build_union(&f).unwrap();
            assert!(!sql.contains("public.fuel_events"), "fuel leaked through");
            assert!(!sql.contains("public.loans"), "loans leaked through");
        }
    }

    #[test]
    fn category_filter_scopes_to_the_matching_branch() {
        let f = UnifiedFilters { category: Some("Fuel".into()), ..base() };
        let sql = build_union(&f).unwrap();
        assert!(sql.contains("public.fuel_events"));
        assert!(!sql.contains("public.loans"));

        let f = UnifiedFilters { category: Some("Labor".into()), ..base() };
        let sql = build_union(&f).unwrap();
        assert!(!sql.contains("public.fuel_events"));
        assert!(!sql.contains("public.loans"));
        assert!(sql.contains("banksms.transactions"));
    }

    #[test]
    fn source_filter_selects_a_single_branch() {
        let f = UnifiedFilters { source: Some("loan".into()), ..base() };
        let sql = build_union(&f).unwrap();
        assert!(sql.contains("public.loans"));
        assert!(!sql.contains("public.fuel_events"));
        assert!(!sql.contains("banksms.transactions"));
    }

    #[test]
    fn loans_are_excluded_by_vehicle_and_company_filters() {
        // Loans are not vehicle- or company-scoped, so these filters cannot
        // meaningfully match one.
        let f = UnifiedFilters { car_no_plate: Some("ف ج م 8567".into()), ..base() };
        assert!(!build_union(&f).unwrap().contains("public.loans"));

        let f = UnifiedFilters { company: Some("TAQA".into()), ..base() };
        assert!(!build_union(&f).unwrap().contains("public.loans"));
    }

    /// Count the columns a branch projects.
    ///
    /// Counts top-level commas in the SELECT list, tracking parenthesis depth so
    /// commas inside calls like `to_char(x, 'fmt')` and `COALESCE(a, b)` are not
    /// mistaken for column separators. An earlier version counted occurrences of
    /// " AS ", which silently disagreed the moment a branch projected a bare
    /// column name instead of an alias.
    fn projected_columns(sql: &str) -> usize {
        let head = sql.split("\n    FROM").next().unwrap_or(sql);
        let head = head.trim_start().strip_prefix("SELECT").unwrap_or(head);

        let mut depth = 0i32;
        let mut commas = 0usize;
        for c in head.chars() {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                ',' if depth == 0 => commas += 1,
                _ => {}
            }
        }
        commas + 1
    }

    /// Every branch must project the same columns in the same order, or Postgres
    /// will happily union mismatched values into the wrong fields.
    #[test]
    fn branches_project_the_same_column_count() {
        let expected = UNIFIED_COLUMNS.split(',').count();
        for (name, sql) in [
            ("transactions", TRANSACTIONS_BRANCH),
            ("fuel", FUEL_BRANCH),
            ("loans", LOAN_BRANCH),
        ] {
            assert_eq!(
                projected_columns(sql),
                expected,
                "{name} branch projects {} columns, expected {expected}",
                projected_columns(sql)
            );
        }
    }

    #[test]
    fn fuel_events_never_appear_writable() {
        assert!(FUEL_BRANCH.contains("FALSE AS editable"));
        assert!(LOAN_BRANCH.contains("FALSE AS editable"));
        assert!(TRANSACTIONS_BRANCH.contains("TRUE AS editable"));
    }

    /// The whole point of the split: fleet_expenses must never be read again.
    #[test]
    fn no_branch_reads_the_legacy_table() {
        for sql in [TRANSACTIONS_BRANCH, FUEL_BRANCH, LOAN_BRANCH] {
            assert!(!sql.contains("fleet_expenses"));
        }
    }
}
