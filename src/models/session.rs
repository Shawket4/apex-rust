use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc, NaiveDateTime};

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
    #[serde(serialize_with = "serialize_naive_datetime")]
    pub time_stamp: NaiveDateTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
}

// Serialize NaiveDateTime as ISO 8601 string
fn serialize_naive_datetime<S>(dt: &NaiveDateTime, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&dt.format("%Y-%m-%dT%H:%M:%S%.fZ").to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SessionLocationSummary {
    pub session_id: i64,  // location_pings.session_id = bigint
    pub total_pings: i64,
    pub first_ping_time: Option<NaiveDateTime>,
    pub last_ping_time: Option<NaiveDateTime>,
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
    #[serde(serialize_with = "serialize_option_naive_datetime")]
    pub first_ping_time: Option<NaiveDateTime>,
    #[serde(serialize_with = "serialize_option_naive_datetime")]
    pub last_ping_time: Option<NaiveDateTime>,
    pub avg_speed: Option<f64>,
    pub max_speed: Option<f64>,
    pub bounding_box: BoundingBox,
}

// Serialize Option<NaiveDateTime> as ISO 8601 string
fn serialize_option_naive_datetime<S>(dt: &Option<NaiveDateTime>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match dt {
        Some(dt) => serializer.serialize_str(&dt.format("%Y-%m-%dT%H:%M:%S%.fZ").to_string()),
        None => serializer.serialize_none(),
    }
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