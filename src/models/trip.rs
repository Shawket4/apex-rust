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
    pub total_trips: i64,
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
}