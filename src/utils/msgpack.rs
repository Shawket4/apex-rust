use actix_web::{HttpResponse, http::header};
use serde::Serialize;
use anyhow::Result;

pub fn msgpack_response<T: Serialize>(data: &T) -> Result<HttpResponse> {
    // Serialize as named fields (maps) instead of arrays
    let mut buf = Vec::new();
    data.serialize(&mut rmp_serde::Serializer::new(&mut buf).with_struct_map())?;
    
    Ok(HttpResponse::Ok()
        .insert_header((header::CONTENT_TYPE, "application/msgpack"))
        .body(buf))
}

pub fn json_response<T: Serialize>(data: &T) -> Result<HttpResponse> {
    Ok(HttpResponse::Ok().json(data))
}

pub fn response<T: Serialize>(data: &T, use_msgpack: bool) -> Result<HttpResponse> {
    if use_msgpack {
        msgpack_response(data)
    } else {
        json_response(data)
    }
}