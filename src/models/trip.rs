use serde::{Deserialize, Serialize};

// ============================================================================
// Core Trip Statistics Models
// ============================================================================

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TripStatisticsDetails {
    pub group_name: String,
    pub total_trips: i64,
    pub total_volume: f64,
    pub total_distance: f64,
    pub total_revenue: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub car_rental: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_with_vat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distinct_cars: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distinct_days: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub car_days: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CarStats {
    pub car_no_plate: String,
    pub total_trips: i64,
    pub total_volume: f64,
    pub total_distance: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_revenue: Option<f64>,
    pub working_days: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub car_rental: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_with_vat: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RouteRevenueStats {
    pub route_name: String,
    pub total_trips: i64,
    pub total_volume: f64,
    pub total_distance: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_revenue: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub car_rental: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_with_vat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee: Option<f64>,
    pub route_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drop_off_point: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_category: Option<i32>,
    pub cars: Vec<CarStats>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TripStatistics {
    pub company: String,
    /// Logical trips: a multi-container trip counts ONCE, however many receipts
    /// it carries. Computed in one pass over the whole filtered set -- summing
    /// per-group counts double-counts any trip whose containers span more than
    /// one drop-off point, which is what this field used to do.
    pub total_trips: i64,
    /// Individual receipt rows. Always >= total_trips.
    pub total_receipts: i64,
    pub total_volume: f64,
    pub total_distance: f64,
    pub total_revenue: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_car_rent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_vat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_amount: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Vec<TripStatisticsDetails>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_details: Option<Vec<RouteRevenueStats>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CompanyRevenueDetails {
    pub company: String,
    pub total_trips: i64,
    pub total_volume: f64,
    pub total_distance: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_revenue: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub car_rental: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_with_vat: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TripRevenueDateResponse {
    pub date: String,
    pub total_trips: i64,
    pub total_volume: f64,
    pub total_distance: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_revenue: Option<f64>,
    pub company_details: Vec<CompanyRevenueDetails>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CarTotal {
    pub car_no_plate: String,
    pub liters: f64,
    pub distance: f64,
    pub base_revenue: f64,
    pub vat: f64,
    pub rent: f64,
}

/// [`CarTotal`] per driver: the same stored rows, grouped by driver instead of
/// plate, so the two lists add up to the same figures. An empty `driver_name`
/// collects the rows that recorded no driver.
#[derive(Debug, Serialize, Deserialize)]
pub struct DriverTotal {
    /// `None` for trips that recorded no driver ID and matched no driver by name.
    pub driver_id: Option<i64>,
    /// The driver's current name.
    pub driver_name: String,
    /// Every other name this driver's trips recorded: earlier names, typos.
    pub other_names: Vec<String>,
    pub liters: f64,
    pub distance: f64,
    pub base_revenue: f64,
    pub vat: f64,
    pub rent: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TripStatisticsResponse {
    pub message: String,
    pub data: Vec<TripStatistics>,
    #[serde(rename = "statsByDate")]
    pub stats_by_date: Vec<TripRevenueDateResponse>,
    #[serde(rename = "hasFinancialAccess")]
    pub has_financial_access: bool,
    #[serde(rename = "carTotals")]
    pub car_totals: Vec<CarTotal>,
    #[serde(rename = "driverTotals")]
    pub driver_totals: Vec<DriverTotal>,
}
