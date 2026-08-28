//! Wire shapes for the trips list.
//!
//! These mirror FalconGo's GORM structs field for field, because the dashboard
//! is already parsing that JSON and the move to Rust is meant to be invisible
//! to it. Two consequences worth knowing before editing anything here:
//!
//! * `gorm.Model` serialises its embedded fields in Go's exported casing —
//!   `ID`, `CreatedAt`, `UpdatedAt`, `DeletedAt` — while every hand-tagged
//!   field is snake_case. The mix looks like a mistake and is not; the
//!   dashboard's zod schemas read `ID` and would drop a row spelled `id`.
//! * Timestamps are `timestamp without time zone` in Postgres, so they are
//!   `NaiveDateTime` here. FalconGo writes them in Africa/Cairo wall-clock and
//!   the dashboard renders them as such; attaching a UTC offset on the way out
//!   would shift every receipt step by two or three hours.

use chrono::NaiveDateTime;
use serde::Serialize;

/// A step in a receipt's journey from the garage to the office.
#[derive(Debug, Clone, Serialize)]
pub struct ReceiptStep {
    #[serde(rename = "ID")]
    pub id: i64,
    #[serde(rename = "CreatedAt")]
    pub created_at: Option<NaiveDateTime>,
    #[serde(rename = "UpdatedAt")]
    pub updated_at: Option<NaiveDateTime>,
    #[serde(rename = "DeletedAt")]
    pub deleted_at: Option<NaiveDateTime>,

    pub trip_id: i64,
    /// "Garage" or "Office".
    pub location: String,
    pub received_by: String,
    pub received_at: NaiveDateTime,
    pub step_order: i64,
    pub stamped: bool,
    pub notes: String,
}

/// A scanned receipt image.
#[derive(Debug, Clone, Serialize)]
pub struct Receipt {
    pub id: i64,
    pub batch_id: i64,
    pub image_path: String,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

/// The driver on a receipt batch. Only the fields the dashboard reads — the
/// `drivers` row also carries licence scans and a PIN, and a list endpoint has
/// no business handing those out.
#[derive(Debug, Clone, Serialize)]
pub struct BatchDriver {
    #[serde(rename = "ID")]
    pub id: i64,
    pub name: Option<String>,
}

/// A batch of receipt images scanned against a parent trip.
#[derive(Debug, Clone, Serialize)]
pub struct ReceiptBatch {
    pub id: i64,
    pub driver_id: i64,
    pub status: Option<String>,
    pub scanned_at: Option<NaiveDateTime>,
    pub assigned_to_trip_id: Option<i64>,
    pub receipts: Vec<Receipt>,
    #[serde(rename = "Driver", skip_serializing_if = "Option::is_none")]
    pub driver: Option<BatchDriver>,
}

/// The header row of a multi-container trip.
#[derive(Debug, Clone, Serialize)]
pub struct ParentTrip {
    #[serde(rename = "ID")]
    pub id: i64,
    #[serde(rename = "CreatedAt")]
    pub created_at: Option<NaiveDateTime>,
    #[serde(rename = "UpdatedAt")]
    pub updated_at: Option<NaiveDateTime>,

    pub car_id: i64,
    pub driver_id: i64,
    pub car_no_plate: String,
    pub driver_name: String,
    pub transporter: String,
    pub tank_capacity: i64,
    pub company: String,
    pub terminal: String,
    pub date: String,
    pub author: Option<String>,
    pub overwriter: Option<String>,
    pub session_id: Option<i64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt_batch: Option<ReceiptBatch>,
}

/// One row of the trips list: a standalone trip, or one container of a
/// multi-container trip.
#[derive(Debug, Clone, Serialize)]
pub struct TripListRow {
    #[serde(rename = "ID")]
    pub id: i64,
    #[serde(rename = "CreatedAt")]
    pub created_at: Option<NaiveDateTime>,
    #[serde(rename = "UpdatedAt")]
    pub updated_at: Option<NaiveDateTime>,
    #[serde(rename = "DeletedAt")]
    pub deleted_at: Option<NaiveDateTime>,

    pub parent_trip_id: Option<i64>,
    pub car_id: i64,
    pub driver_id: i64,
    pub car_no_plate: String,
    pub driver_name: String,
    pub transporter: String,
    pub tank_capacity: i64,

    pub company: String,
    pub terminal: String,
    pub drop_off_point: String,
    pub location_name: String,
    pub capacity: i64,
    pub gas_type: String,

    pub date: String,
    pub receipt_no: String,
    pub mileage: f64,

    /// From the route's fee mapping, not stored on the trip.
    pub distance: f64,
    /// Also from the fee mapping. For Watanya this is a BAND NUMBER (1..15),
    /// not a rate — the dashboard labels it accordingly.
    pub fee: f64,

    /* ---- revenue ---------------------------------------------------------
     * `revenue` is the trip's own earnings and is a fact about the trip.
     * The three `allocated_*` fields are a SHARE of costs the trip does not
     * own alone: TAQA's rental is earned per car per month and Petromin's per
     * car-day, spread across the rows that incurred them. They sum to the
     * statistics total for the same window, and they move when the window
     * moves — which is why they are named as allocations rather than folded
     * silently into `revenue`.
     *
     * All four are omitted entirely below permission 4. A caller who may not
     * see money does not receive a zero, which would read as "this trip earned
     * nothing"; they receive no field at all.
     * -------------------------------------------------------------------- */
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revenue: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allocated_rental: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allocated_vat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allocated_total: Option<f64>,

    pub receipt_steps: Vec<ReceiptStep>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_trip: Option<ParentTrip>,
}

/// Pagination envelope, matching FalconGo's `meta` block.
#[derive(Debug, Clone, Serialize)]
pub struct TripListMeta {
    pub total: i64,
    pub page: i64,
    pub limit: i64,
    pub pages: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TripListResponse {
    pub message: &'static str,
    pub data: Vec<TripListRow>,
    pub meta: TripListMeta,
}
