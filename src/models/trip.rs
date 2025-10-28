use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, FromRow)]
pub struct Trip {
    pub id: i32,
    pub company: String,
    pub terminal: String,
    pub drop_off_point: String,
    pub car_no_plate: String,
    pub driver_name: Option<String>,
    pub date: String,
    pub tank_capacity: i32,
    pub parent_trip_id: Option<i32>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TripStatisticsDetails {
    pub group_name: String,
    pub total_trips: i64,
    pub total_volume: f64,
    pub total_distance: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_revenue: Option<f64>,
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
    pub details: Vec<TripStatisticsDetails>,
    pub route_details: Vec<RouteRevenueStats>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CarTotal {
    pub car_no_plate: String,
    pub liters: f64,
    pub distance: f64,
    pub base_revenue: f64,
    pub vat: f64,
    pub rent: f64,
}
