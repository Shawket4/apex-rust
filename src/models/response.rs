use serde::{Deserialize, Serialize};
use super::trip::{TripStatistics, CarTotal};

#[derive(Debug, Serialize, Deserialize)]
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

#[derive(Debug, Serialize, Deserialize)]
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
pub struct TripStatisticsResponse {
    pub message: String,
    pub data: Vec<TripStatistics>,
    pub stats_by_date: Vec<TripRevenueDateResponse>,
    pub has_financial_access: bool,
    pub car_totals: Vec<CarTotal>,
}
