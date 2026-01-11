use sqlx::PgPool;
use anyhow::Result;
use crate::models::session::*;

/// Check if session exists and get driver_id for permission check
pub async fn get_session_for_permission_check(
    pool: &PgPool,
    session_id: i32,
) -> Result<Option<SessionPermissionCheck>> {
    let session = sqlx::query_as::<_, SessionPermissionCheck>(
        "SELECT id, driver_id FROM driver_sessions WHERE id = $1"
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;
    
    Ok(session)
}

/// Get pre-computed stats from materialized view (fast path)
pub async fn get_session_stats_from_view(
    pool: &PgPool,
    session_id: i32,
) -> Result<Option<SessionLocationSummary>> {
    // Check if materialized view exists first
    let view_exists: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT FROM pg_matviews 
            WHERE matviewname = 'session_location_summary'
        )
        "#
    )
    .fetch_one(pool)
    .await?;
    
    if !view_exists {
        return Ok(None);
    }
    
    let summary = sqlx::query_as::<_, SessionLocationSummary>(
        r#"
        SELECT 
            session_id, total_pings, first_ping_time, last_ping_time,
            avg_speed, max_speed, min_lat, max_lat, min_lng, max_lng
        FROM session_location_summary
        WHERE session_id = $1
        "#
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;
    
    Ok(summary)
}

/// Get total ping count (fallback when no materialized view)
pub async fn get_ping_count(pool: &PgPool, session_id: i32) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM location_pings WHERE session_id = $1"
    )
    .bind(session_id)
    .fetch_one(pool)
    .await?;
    
    Ok(count)
}

/// Fetch all location pings for a session - optimized query
/// Uses raw query for maximum performance
pub async fn fetch_location_pings(
    pool: &PgPool,
    session_id: i32,
) -> Result<Vec<LocationPingLite>> {
    let pings = sqlx::query_as::<_, LocationPingLite>(
        r#"
        SELECT id, lat, lng, time_stamp, speed
        FROM location_pings
        WHERE session_id = $1
        ORDER BY time_stamp ASC
        "#
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;
    
    Ok(pings)
}

/// Fetch location pings with server-side downsampling using NTILE
/// This is faster than fetching all and downsampling in memory for very large datasets
pub async fn fetch_location_pings_downsampled(
    pool: &PgPool,
    session_id: i32,
    target_count: i32,
) -> Result<Vec<LocationPingLite>> {
    // Use NTILE to divide into buckets and pick first from each
    let pings = sqlx::query_as::<_, LocationPingLite>(
        r#"
        WITH ranked AS (
            SELECT 
                id, lat, lng, time_stamp, speed,
                NTILE($2) OVER (ORDER BY time_stamp ASC) as bucket,
                ROW_NUMBER() OVER (PARTITION BY NTILE($2) OVER (ORDER BY time_stamp ASC) ORDER BY time_stamp ASC) as rn
            FROM location_pings
            WHERE session_id = $1
        )
        SELECT id, lat, lng, time_stamp, speed
        FROM ranked
        WHERE rn = 1
        ORDER BY time_stamp ASC
        "#
    )
    .bind(session_id)
    .bind(target_count)
    .fetch_all(pool)
    .await?;
    
    Ok(pings)
}