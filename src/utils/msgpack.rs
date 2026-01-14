use actix_web::{HttpResponse, http::header};
use serde::Serialize;
use anyhow::Result;

/// Serialize to MessagePack with pre-allocated buffer
/// For large payloads, pre-allocation reduces reallocations significantly
pub fn msgpack_response<T: Serialize>(data: &T) -> Result<HttpResponse> {
    // Pre-allocate buffer (adjust based on typical response size)
    let mut buf = Vec::with_capacity(64 * 1024); // 64KB initial
    
    // Use struct_map for named fields (readable) vs struct_tuple (smaller)
    data.serialize(&mut rmp_serde::Serializer::new(&mut buf).with_struct_map())?;
    
    Ok(HttpResponse::Ok()
        .insert_header((header::CONTENT_TYPE, "application/msgpack"))
        .insert_header(("X-Content-Encoding", "msgpack"))
        .body(buf))
}

/// Compact MessagePack - uses arrays instead of maps (smaller but less readable)
/// ~30-40% smaller than struct_map for location data
pub fn msgpack_compact_response<T: Serialize>(data: &T) -> Result<HttpResponse> {
    let mut buf = Vec::with_capacity(64 * 1024);
    
    // Struct as tuple = array format [val1, val2, ...] instead of {"key": val}
    data.serialize(&mut rmp_serde::Serializer::new(&mut buf))?;
    
    Ok(HttpResponse::Ok()
        .insert_header((header::CONTENT_TYPE, "application/msgpack"))
        .insert_header(("X-Content-Encoding", "msgpack-compact"))
        .body(buf))
}

pub fn json_response<T: Serialize>(data: &T) -> Result<HttpResponse> {
    Ok(HttpResponse::Ok().json(data))
}

/// Smart response - auto-detects format from query param
pub fn response<T: Serialize>(data: &T, use_msgpack: bool) -> Result<HttpResponse> {
    if use_msgpack {
        msgpack_response(data)
    } else {
        json_response(data)
    }
}

// ============================================================================
// For even faster serialization of location pings specifically:
// ============================================================================

use crate::models::session::LocationPingLite;

/// Ultra-fast custom serialization for location pings
/// Bypasses generic serialization for ~2x speedup on large arrays
pub fn serialize_pings_fast(pings: &[LocationPingLite]) -> Vec<u8> {
    use rmp::encode::*;
    
    // Estimate: ~50 bytes per ping
    let mut buf = Vec::with_capacity(pings.len() * 50);
    
    // Write array header
    let _ = write_array_len(&mut buf, pings.len() as u32);
    
    for ping in pings {
        // Write as 5-element map
        let _ = write_map_len(&mut buf, 5);
        
        // id
        let _ = write_str(&mut buf, "id");
        let _ = write_i32(&mut buf, ping.id);
        
        // lat
        let _ = write_str(&mut buf, "lat");
        let _ = write_f64(&mut buf, ping.lat);
        
        // lng
        let _ = write_str(&mut buf, "lng");
        let _ = write_f64(&mut buf, ping.lng);
        
        // time_stamp (as Unix timestamp for compactness)
        let _ = write_str(&mut buf, "ts");
        let _ = write_i64(&mut buf, ping.time_stamp.and_utc().timestamp());
        
        // speed (optional)
        let _ = write_str(&mut buf, "spd");
        match ping.speed {
            Some(s) => { let _ = write_f64(&mut buf, s); },
            None => { let _ = write_nil(&mut buf); },
        }
    }
    
    buf
}