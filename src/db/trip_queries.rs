//! The trips list, moved off FalconGo.
//!
//! This replaces `GetAllTrips` in `Controllers/trip.go`. It is a faithful port
//! — same filters, same ordering, same pagination envelope, same sibling
//! backfill — with three deliberate differences:
//!
//! 1. **Revenue is on every row.** That is the point of the move: the formulas
//!    live in [`crate::db::revenue`] and are now reachable from the list
//!    without a second copy. See [`crate::db::revenue::allocation`] for what
//!    "allocated" means and why the totals reconcile with statistics.
//! 2. **The fee mapping is joined, not looked up per row.** FalconGo issued one
//!    `SELECT` per trip after fetching the page; a 200-row page cost 201 round
//!    trips. Here it is one join inside the revenue CTE.
//! 3. **Search is case-insensitive.** FalconGo used `LIKE`, so searching a
//!    plate in the wrong case silently returned nothing.
//!
//! ## Parameter numbering
//!
//! Every filter is bound at a FIXED position and guarded with
//! `$n::type IS NULL OR ...` rather than being concatenated in when present.
//! Dynamically assembled parameter lists are how this service has already shipped
//! two production 500s from renumbered slots. The positions are named in
//! [`params`] and the binding order in [`bind_filters`] is the only place that
//! has to agree with them.

use anyhow::Result;
use sqlx::{postgres::PgArguments, query::Query, PgPool, Postgres, Row};
use std::collections::{HashMap, HashSet};

use crate::db::revenue::allocation::per_row_revenue_cte;
use crate::models::trip_list::*;

/// Bind positions, in the order [`bind_filters`] pushes them.
mod params {
    pub const COMPANY: &str = "$1";
    pub const FROM: &str = "$2";
    pub const TO: &str = "$3";
    pub const SEARCH: &str = "$4";
    pub const MISSING_DATA: &str = "$5";
    pub const RECEIPT_STATUS: &str = "$6";
    /// Pagination is bound after the filters, so it can be omitted from the
    /// count query without disturbing anything above it.
    pub const LIMIT: &str = "$7";
    pub const OFFSET: &str = "$8";
}

/// The largest page the endpoint will serve. FalconGo had no cap at all, so a
/// client asking for `limit=100000` pulled the whole table plus every nested
/// receipt in one request.
pub const MAX_LIMIT: i64 = 200;
pub const DEFAULT_LIMIT: i64 = 10;

#[derive(Debug, Clone, Default)]
pub struct TripListFilters {
    pub page: i64,
    pub limit: i64,
    pub search: Option<String>,
    /// "driver" | "route" | "any"
    pub missing_data: Option<String>,
    /// "pending" | "in_garage" | "in_office"
    pub receipt_status: Option<String>,
    /// Scopes the revenue window as well as the rows.
    pub company: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
}

impl TripListFilters {
    /// Clamps pagination into range. A page of zero or a negative limit is a
    /// client bug, and answering it with an error helps nobody.
    pub fn normalized(mut self) -> Self {
        self.page = self.page.max(1);
        self.limit = self.limit.clamp(1, MAX_LIMIT);
        self
    }

    fn offset(&self) -> i64 {
        (self.page - 1) * self.limit
    }

    /// `%term%`, or None. An all-whitespace search is not a search.
    fn search_pattern(&self) -> Option<String> {
        let term = self.search.as_deref()?.trim();
        (!term.is_empty()).then(|| format!("%{term}%"))
    }
}

/* ------------------------------------------------------------------------ */
/* Predicates                                                                */
/* ------------------------------------------------------------------------ */

/// Scopes the revenue window: date range and company only.
///
/// Deliberately excludes search and receipt status — see the contract on
/// [`per_row_revenue_cte`]. Folding those in would make a trip's allocated
/// rental depend on what the user typed in a search box.
fn window_predicate() -> String {
    use params::*;
    format!(
        "({COMPANY}::text IS NULL OR t.company = {COMPANY}) \
         AND ({FROM}::text IS NULL OR t.date >= {FROM}) \
         AND ({TO}::text IS NULL OR t.date <= {TO})"
    )
}

/// Narrows which of the windowed rows are actually listed. Applied over the
/// `revenue` CTE, aliased `r`.
fn row_predicate() -> String {
    use params::*;
    format!(
        "({SEARCH}::text IS NULL OR ( \
              r.car_no_plate   ILIKE {SEARCH} \
           OR r.driver_name    ILIKE {SEARCH} \
           OR r.drop_off_point ILIKE {SEARCH} \
           OR r.terminal       ILIKE {SEARCH} \
           OR r.date           ILIKE {SEARCH} \
           OR r.receipt_no     ILIKE {SEARCH} \
           OR r.tank_capacity::text ILIKE {SEARCH} \
         )) \
         AND ({MISSING_DATA}::text IS NULL OR CASE {MISSING_DATA} \
                WHEN 'driver' THEN r.driver_name = '{UNSET}' \
                WHEN 'route'  THEN r.drop_off_point = '{UNSET}' \
                WHEN 'any'    THEN r.driver_name = '{UNSET}' OR r.drop_off_point = '{UNSET}' \
                ELSE TRUE END) \
         AND ({RECEIPT_STATUS}::text IS NULL OR CASE {RECEIPT_STATUS} \
                WHEN 'pending'   THEN NOT EXISTS ( \
                    SELECT 1 FROM receipt_steps rs \
                    WHERE rs.trip_id = r.id AND rs.deleted_at IS NULL) \
                WHEN 'in_garage' THEN {latest_garage} \
                WHEN 'in_office' THEN {latest_office} \
                ELSE TRUE END)",
        UNSET = UNSET_MARKER,
        latest_garage = latest_step_is("Garage"),
        latest_office = latest_step_is("Office"),
    )
}

/// What FalconGo writes into a trip whose driver or route was never filled in.
/// Arabic for "not registered"; it is a sentinel value in the data, not a
/// translation concern, which is why it is matched literally.
const UNSET_MARKER: &str = "غير مسجل";

/// True when the most recent receipt step for the row is at `location`.
///
/// Phrased as "a step here with no later step anywhere" rather than an
/// ORDER BY ... LIMIT 1 comparison, which is how FalconGo phrased it and is
/// also what lets the index on `trip_id` do the work.
fn latest_step_is(location: &str) -> String {
    // deleted_at IS NULL in BOTH subqueries: a soft-deleted step is a
    // correction, and letting it count as "the latest" resurrects the very
    // state someone deleted (observed: a deleted Garage step later than the
    // live Office step made an office-received trip filter as in_garage).
    format!(
        "EXISTS (SELECT 1 FROM receipt_steps rs1 \
          WHERE rs1.trip_id = r.id AND rs1.deleted_at IS NULL \
            AND rs1.location = '{location}' \
            AND NOT EXISTS (SELECT 1 FROM receipt_steps rs2 \
                 WHERE rs2.trip_id = r.id AND rs2.deleted_at IS NULL \
                   AND rs2.received_at > rs1.received_at))"
    )
}

/// Binds the six filters, in the order [`params`] names them.
fn bind_filters<'q>(
    q: Query<'q, Postgres, PgArguments>,
    f: &'q TripListFilters,
    search: &'q Option<String>,
) -> Query<'q, Postgres, PgArguments> {
    q.bind(f.company.as_deref())
        .bind(f.from.as_deref())
        .bind(f.to.as_deref())
        .bind(search.as_deref())
        .bind(f.missing_data.as_deref())
        .bind(f.receipt_status.as_deref())
}

/* ------------------------------------------------------------------------ */
/* The list                                                                  */
/* ------------------------------------------------------------------------ */

/// One page of trips, plus the total matching the filters.
///
/// `financial` gates every money field. When false the revenue columns are not
/// merely zeroed for display — they are left off the payload entirely, so a
/// caller without permission never receives a figure to begin with.
///
/// The returned vector can be LONGER than `limit`: if the page contains a
/// container of a multi-container trip, every sibling container comes with it.
/// FalconGo did the same, and for good reason — rendering three of a trip's
/// four containers because the fourth fell past the page boundary shows a total
/// that is simply wrong. `meta.total` still counts filtered rows, so the page
/// arithmetic is unaffected.
pub async fn list_trips(
    pool: &PgPool,
    filters: &TripListFilters,
    financial: bool,
) -> Result<(Vec<TripListRow>, i64)> {
    let search = filters.search_pattern();
    let cte = per_row_revenue_cte(&window_predicate());
    let rows_where = row_predicate();

    /* ---- total ---- */
    let count_sql = format!("WITH {cte} SELECT COUNT(*) AS n FROM revenue r WHERE {rows_where}");
    let total: i64 = bind_filters(sqlx::query(&count_sql), filters, &search)
        .fetch_one(pool)
        .await?
        .get("n");

    /* ---- the page itself ---- */
    let page_sql = format!(
        "WITH {cte} \
         SELECT r.* FROM revenue r \
         WHERE {rows_where} \
         ORDER BY r.date DESC, r.receipt_no DESC, r.id DESC \
         LIMIT {limit} OFFSET {offset}",
        limit = params::LIMIT,
        offset = params::OFFSET,
    );
    let page = bind_filters(sqlx::query(&page_sql), filters, &search)
        .bind(filters.limit)
        .bind(filters.offset())
        .fetch_all(pool)
        .await?;

    let mut trips: Vec<TripListRow> = page.iter().map(|r| map_trip(r, financial)).collect();

    /* ---- sibling containers ---- */
    //
    // Any parent represented on this page brings ALL of its containers, even
    // ones the page boundary cut off. The revenue CTE is rebuilt over the same
    // window so the siblings' allocated shares match their page-mates exactly;
    // computing them over a narrower window would give two containers of one
    // trip two different shares of the same car-day.
    let parent_ids: Vec<i64> = trips
        .iter()
        .filter_map(|t| t.parent_trip_id.filter(|id| *id > 0))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    if !parent_ids.is_empty() {
        let seen: HashSet<i64> = trips.iter().map(|t| t.id).collect();
        // Only the window parameters are bound here: the row filters are
        // deliberately NOT applied, because a sibling is included on account of
        // its parent, not on account of matching the search. Postgres rejects a
        // bind list longer than the placeholders actually used, so the array
        // takes the first slot after the window's three.
        let sibling_sql = format!(
            "WITH {cte} \
             SELECT r.* FROM revenue r \
             WHERE r.parent_trip_id = ANY($4) \
             ORDER BY r.date DESC, r.receipt_no DESC, r.id DESC"
        );
        let siblings = sqlx::query(&sibling_sql)
            .bind(filters.company.as_deref())
            .bind(filters.from.as_deref())
            .bind(filters.to.as_deref())
            .bind(&parent_ids)
            .fetch_all(pool)
            .await?;

        for row in &siblings {
            let trip = map_trip(row, financial);
            if !seen.contains(&trip.id) {
                trips.push(trip);
            }
        }
        // Keep the sort stable now that siblings have been spliced in.
        trips.sort_by(|a, b| {
            b.date
                .cmp(&a.date)
                .then_with(|| b.receipt_no.cmp(&a.receipt_no))
                .then_with(|| b.id.cmp(&a.id))
        });
    }

    if trips.is_empty() {
        return Ok((trips, total));
    }

    attach_receipt_steps(pool, &mut trips).await?;
    attach_parent_trips(pool, &mut trips).await?;

    Ok((trips, total))
}

/* ------------------------------------------------------------------------ */
/* Row mapping                                                               */
/* ------------------------------------------------------------------------ */

fn map_trip(row: &sqlx::postgres::PgRow, financial: bool) -> TripListRow {
    // Financial columns are read only when the caller may see them, so an
    // unauthorised response has no field rather than a zero that would read as
    // "this trip earned nothing".
    let money = |name: &str| financial.then(|| row.try_get::<f64, _>(name).unwrap_or(0.0));

    TripListRow {
        id: row.get::<i32, _>("id") as i64,
        created_at: row.try_get("created_at").ok(),
        updated_at: row.try_get("updated_at").ok(),
        deleted_at: row.try_get("deleted_at").ok(),

        parent_trip_id: row.try_get("parent_trip_id").ok().flatten(),
        car_id: row.try_get::<Option<i64>, _>("car_id").ok().flatten().unwrap_or(0),
        driver_id: row
            .try_get::<Option<i64>, _>("driver_id")
            .ok()
            .flatten()
            .unwrap_or(0),
        car_no_plate: opt_str(row, "car_no_plate"),
        driver_name: opt_str(row, "driver_name"),
        transporter: opt_str(row, "transporter"),
        tank_capacity: row
            .try_get::<Option<i64>, _>("tank_capacity")
            .ok()
            .flatten()
            .unwrap_or(0),

        company: opt_str(row, "company"),
        terminal: opt_str(row, "terminal"),
        drop_off_point: opt_str(row, "drop_off_point"),
        location_name: opt_str(row, "location_name"),
        capacity: row
            .try_get::<Option<i64>, _>("capacity")
            .ok()
            .flatten()
            .unwrap_or(0),
        gas_type: opt_str(row, "gas_type"),

        date: opt_str(row, "date"),
        receipt_no: opt_str(row, "receipt_no"),
        mileage: numeric_f64(row, "mileage"),

        distance: row.try_get::<f64, _>("fee_distance").unwrap_or(0.0),
        fee: row.try_get::<f64, _>("fee_value").unwrap_or(0.0),

        revenue: money("base_revenue"),
        allocated_rental: money("allocated_rental"),
        allocated_vat: money("allocated_vat"),
        allocated_total: money("allocated_total"),

        receipt_steps: Vec::new(),
        parent_trip: None,
    }
}

fn opt_str(row: &sqlx::postgres::PgRow, name: &str) -> String {
    row.try_get::<Option<String>, _>(name)
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// `mileage` and `revenue` are NUMERIC in the trips table. They are read
/// through `rust_decimal` and converted, rather than asking Postgres for a
/// float8, so a value that does not fit reports itself instead of silently
/// arriving as infinity.
fn numeric_f64(row: &sqlx::postgres::PgRow, name: &str) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    row.try_get::<Option<rust_decimal::Decimal>, _>(name)
        .ok()
        .flatten()
        .and_then(|d| d.to_f64())
        .unwrap_or(0.0)
}

/* ------------------------------------------------------------------------ */
/* Nested loads — one query each, never one per row                          */
/* ------------------------------------------------------------------------ */

async fn attach_receipt_steps(pool: &PgPool, trips: &mut [TripListRow]) -> Result<()> {
    let ids: Vec<i64> = trips.iter().map(|t| t.id).collect();
    let rows = sqlx::query(
        "SELECT id, created_at, updated_at, deleted_at, trip_id, location, \
                received_by, received_at, step_order, stamped, notes \
         FROM receipt_steps \
         WHERE trip_id = ANY($1) AND deleted_at IS NULL \
         ORDER BY trip_id, step_order, received_at",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?;

    let mut by_trip: HashMap<i64, Vec<ReceiptStep>> = HashMap::new();
    for r in &rows {
        let step = ReceiptStep {
            id: r.get::<i32, _>("id") as i64,
            created_at: r.try_get("created_at").ok(),
            updated_at: r.try_get("updated_at").ok(),
            deleted_at: r.try_get("deleted_at").ok(),
            trip_id: r.get("trip_id"),
            location: r.get("location"),
            received_by: r.get("received_by"),
            received_at: r.get("received_at"),
            step_order: r.get("step_order"),
            stamped: r.try_get("stamped").unwrap_or(false),
            notes: r.try_get::<Option<String>, _>("notes").ok().flatten().unwrap_or_default(),
        };
        by_trip.entry(step.trip_id).or_default().push(step);
    }

    for trip in trips.iter_mut() {
        trip.receipt_steps = by_trip.remove(&trip.id).unwrap_or_default();
    }
    Ok(())
}

/// Loads parent trips, their receipt batches, the batches' images and each
/// batch's driver — four queries regardless of page size.
async fn attach_parent_trips(pool: &PgPool, trips: &mut [TripListRow]) -> Result<()> {
    let parent_ids: Vec<i64> = trips
        .iter()
        .filter_map(|t| t.parent_trip_id.filter(|id| *id > 0))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if parent_ids.is_empty() {
        return Ok(());
    }

    let rows = sqlx::query(
        "SELECT id, created_at, updated_at, car_id, driver_id, car_no_plate, \
                driver_name, transporter, tank_capacity, company, terminal, date, \
                author, overwriter, session_id \
         FROM parent_trips WHERE id = ANY($1) AND deleted_at IS NULL",
    )
    .bind(&parent_ids)
    .fetch_all(pool)
    .await?;

    let mut parents: HashMap<i64, ParentTrip> = rows
        .iter()
        .map(|r| {
            let id = r.get::<i32, _>("id") as i64;
            (
                id,
                ParentTrip {
                    id,
                    created_at: r.try_get("created_at").ok(),
                    updated_at: r.try_get("updated_at").ok(),
                    car_id: r.try_get::<Option<i64>, _>("car_id").ok().flatten().unwrap_or(0),
                    driver_id: r
                        .try_get::<Option<i64>, _>("driver_id")
                        .ok()
                        .flatten()
                        .unwrap_or(0),
                    car_no_plate: opt_str(r, "car_no_plate"),
                    driver_name: opt_str(r, "driver_name"),
                    transporter: opt_str(r, "transporter"),
                    tank_capacity: r
                        .try_get::<Option<i64>, _>("tank_capacity")
                        .ok()
                        .flatten()
                        .unwrap_or(0),
                    company: opt_str(r, "company"),
                    terminal: opt_str(r, "terminal"),
                    date: opt_str(r, "date"),
                    author: r.try_get("author").ok().flatten(),
                    overwriter: r.try_get("overwriter").ok().flatten(),
                    session_id: r.try_get("session_id").ok().flatten(),
                    receipt_batch: None,
                },
            )
        })
        .collect();

    attach_receipt_batches(pool, &parent_ids, &mut parents).await?;

    for trip in trips.iter_mut() {
        if let Some(pid) = trip.parent_trip_id.filter(|id| *id > 0) {
            trip.parent_trip = parents.get(&pid).cloned();
        }
    }
    Ok(())
}

async fn attach_receipt_batches(
    pool: &PgPool,
    parent_ids: &[i64],
    parents: &mut HashMap<i64, ParentTrip>,
) -> Result<()> {
    let batch_rows = sqlx::query(
        "SELECT id, driver_id, status, scanned_at, assigned_to_trip_id \
         FROM receipt_batches \
         WHERE assigned_to_trip_id = ANY($1) AND deleted_at IS NULL",
    )
    .bind(parent_ids)
    .fetch_all(pool)
    .await?;
    if batch_rows.is_empty() {
        return Ok(());
    }

    let batch_ids: Vec<i64> = batch_rows.iter().map(|r| r.get::<i32, _>("id") as i64).collect();
    let driver_ids: Vec<i64> = batch_rows
        .iter()
        .map(|r| r.get::<i64, _>("driver_id"))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let receipt_rows = sqlx::query(
        "SELECT id, batch_id, image_path, created_at, updated_at \
         FROM receipts WHERE batch_id = ANY($1) AND deleted_at IS NULL ORDER BY id",
    )
    .bind(&batch_ids)
    .fetch_all(pool)
    .await?;

    let mut images: HashMap<i64, Vec<Receipt>> = HashMap::new();
    for r in &receipt_rows {
        let batch_id: i64 = r.get("batch_id");
        images.entry(batch_id).or_default().push(Receipt {
            id: r.get::<i32, _>("id") as i64,
            batch_id,
            image_path: r.get("image_path"),
            created_at: r.try_get("created_at").ok(),
            updated_at: r.try_get("updated_at").ok(),
        });
    }

    let driver_rows = sqlx::query("SELECT id, name FROM drivers WHERE id = ANY($1)")
        .bind(&driver_ids)
        .fetch_all(pool)
        .await?;
    let drivers: HashMap<i64, BatchDriver> = driver_rows
        .iter()
        .map(|r| {
            let id = r.get::<i32, _>("id") as i64;
            (id, BatchDriver { id, name: r.try_get("name").ok().flatten() })
        })
        .collect();

    for r in &batch_rows {
        let id = r.get::<i32, _>("id") as i64;
        let driver_id: i64 = r.get("driver_id");
        let assigned: Option<i64> = r.try_get("assigned_to_trip_id").ok().flatten();
        let batch = ReceiptBatch {
            id,
            driver_id,
            status: r.try_get("status").ok().flatten(),
            scanned_at: r.try_get("scanned_at").ok().flatten(),
            assigned_to_trip_id: assigned,
            receipts: images.remove(&id).unwrap_or_default(),
            driver: drivers.get(&driver_id).cloned(),
        };
        if let Some(parent) = assigned.and_then(|pid| parents.get_mut(&pid)) {
            parent.receipt_batch = Some(batch);
        }
    }
    Ok(())
}
