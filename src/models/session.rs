use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};

// ============================================================================
// Database Models
// ============================================================================

#[derive(Debug, sqlx::FromRow)]
pub struct SessionPermissionCheck {
    pub id: i32,          // driver_sessions.id = int
    pub driver_id: i64,   // driver_sessions.driver_id = bigint
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct LocationPingLite {
    pub id: i32,          // location_pings.id = int
    pub lat: f64,
    pub lng: f64,
    #[serde(rename = "time_stamp")]
    pub time_stamp: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SessionLocationSummary {
    pub session_id: i64,  // location_pings.session_id = bigint
    pub total_pings: i64,
    pub first_ping_time: Option<DateTime<Utc>>,
    pub last_ping_time: Option<DateTime<Utc>>,
    pub avg_speed: Option<f64>,
    pub max_speed: Option<f64>,
    pub min_lat: Option<f64>,
    pub max_lat: Option<f64>,
    pub min_lng: Option<f64>,
    pub max_lng: Option<f64>,
}

// ============================================================================
// Response Types
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoundingBox {
    pub min_lat: Option<f64>,
    pub max_lat: Option<f64>,
    pub min_lng: Option<f64>,
    pub max_lng: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStats {
    pub total_pings: i64,
    pub first_ping_time: Option<DateTime<Utc>>,
    pub last_ping_time: Option<DateTime<Utc>>,
    pub avg_speed: Option<f64>,
    pub max_speed: Option<f64>,
    pub bounding_box: BoundingBox,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LocationPingsResponse {
    pub pings: Vec<LocationPingLite>,
    pub count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downsampled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<SessionStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_pings: Option<i64>,
    pub source: String,
}

// ============================================================================
// Query Parameters
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct LocationPingsQuery {
    #[serde(default = "default_true")]
    pub stats: bool,
    #[serde(default)]
    pub downsample: usize,
    pub format: Option<String>,
}

fn default_true() -> bool { true }