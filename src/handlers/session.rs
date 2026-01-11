use actix_web::{web, HttpRequest, HttpResponse, HttpMessage};
use sqlx::PgPool;

use crate::auth::Claims;
use crate::models::session::*;
use crate::db::session_queries::*;
use crate::utils::response;

// ============================================================================
// Constants
// ============================================================================

const SERVER_DOWNSAMPLE_THRESHOLD: i64 = 10_000;

// ============================================================================
// LTTB Downsampling Algorithm
// ============================================================================

fn downsample_lttb(pings: Vec<LocationPingLite>, target: usize) -> Vec<LocationPingLite> {
    let len = pings.len();
    if len <= target || target < 3 {
        return pings;
    }

    let mut result = Vec::with_capacity(target);
    result.push(pings[0].clone());
    
    let bucket_size = (len - 2) as f64 / (target - 2) as f64;
    let mut a = 0usize;
    
    for i in 0..(target - 2) {
        let bucket_start = ((i as f64 * bucket_size) + 1.0) as usize;
        let bucket_end = (((i + 1) as f64 * bucket_size) + 1.0).min(len as f64 - 1.0) as usize;
        
        let next_start = bucket_end;
        let next_end = (((i + 2) as f64 * bucket_size) + 1.0).min(len as f64) as usize;
        
        let (avg_lat, avg_lng) = if next_end > next_start {
            let sum: (f64, f64) = pings[next_start..next_end]
                .iter()
                .fold((0.0, 0.0), |acc, p| (acc.0 + p.lat, acc.1 + p.lng));
            let count = (next_end - next_start) as f64;
            (sum.0 / count, sum.1 / count)
        } else {
            (pings[len - 1].lat, pings[len - 1].lng)
        };
        
        let mut max_area = -1.0f64;
        let mut max_idx = bucket_start;
        
        let point_a = &pings[a];
        for j in bucket_start..=bucket_end.min(len - 1) {
            let point = &pings[j];
            let area = ((point_a.lat - avg_lat) * (point.lng - point_a.lng)
                - (point_a.lat - point.lat) * (avg_lng - point_a.lng))
                .abs();
            
            if area > max_area {
                max_area = area;
                max_idx = j;
            }
        }
        
        result.push(pings[max_idx].clone());
        a = max_idx;
    }
    
    result.push(pings[len - 1].clone());
    result
}

// ============================================================================
// Handler
// ============================================================================

pub async fn get_session_location_pings(
    pool: web::Data<PgPool>,
    path: web::Path<i32>,
    query: web::Query<LocationPingsQuery>,
    req: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    let session_id = path.into_inner();
    let use_msgpack = query.format.as_deref() == Some("msgpack");
    
    // 1. Permission check
    let claims = req.extensions().get::<Claims>().cloned()
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Authentication required"))?;
    
    let session = get_session_for_permission_check(pool.get_ref(), session_id)
        .await
        .map_err(actix_web::error::ErrorInternalServerError)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Session not found"))?;
    
    // Driver can only view their own sessions
    if claims.is_driver() {
        if let Some(driver_id) = claims.driver_id {
            if session.driver_id != driver_id {
                return Err(actix_web::error::ErrorForbidden(
                    "You don't have permission to view this session"
                ));
            }
        }
    }
    
    // 2. Get stats (if requested)
    let mut stats: Option<SessionStats> = None;
    let mut total_pings: Option<i64> = None;
    let mut source = "direct_query".to_string();
    
    if query.stats {
        if let Ok(Some(summary)) = get_session_stats_from_view(pool.get_ref(), session_id).await {
            stats = Some(SessionStats {
                total_pings: summary.total_pings,
                first_ping_time: summary.first_ping_time,
                last_ping_time: summary.last_ping_time,
                avg_speed: summary.avg_speed,
                max_speed: summary.max_speed,
                bounding_box: BoundingBox {
                    min_lat: summary.min_lat,
                    max_lat: summary.max_lat,
                    min_lng: summary.min_lng,
                    max_lng: summary.max_lng,
                },
            });
            total_pings = Some(summary.total_pings);
            source = "materialized_view".to_string();
        } else {
            total_pings = Some(
                get_ping_count(pool.get_ref(), session_id)
                    .await
                    .unwrap_or(0)
            );
        }
    }
    
    // 3. Fetch pings
    let downsample_target = query.downsample;
    let known_count = total_pings.unwrap_or(0);
    
    let (pings, original_count) = if downsample_target > 0 && known_count > SERVER_DOWNSAMPLE_THRESHOLD {
        let pings = fetch_location_pings_downsampled(
            pool.get_ref(), 
            session_id, 
            downsample_target as i32
        )
        .await
        .map_err(actix_web::error::ErrorInternalServerError)?;
        
        (pings, Some(known_count as usize))
    } else {
        let mut pings = fetch_location_pings(pool.get_ref(), session_id)
            .await
            .map_err(actix_web::error::ErrorInternalServerError)?;
        
        let original = pings.len();
        
        if downsample_target > 0 && original > downsample_target {
            pings = downsample_lttb(pings, downsample_target);
            (pings, Some(original))
        } else {
            (pings, None)
        }
    };
    
    // 4. Build response
    let include_total_pings = stats.is_none();
    let response_data = LocationPingsResponse {
        count: pings.len(),
        pings,
        original_count,
        downsampled: original_count.map(|_| true),
        stats,
        total_pings: if include_total_pings { total_pings } else { None },
        source,
    };
    
    response(&response_data, use_msgpack)
        .map_err(actix_web::error::ErrorInternalServerError)
}