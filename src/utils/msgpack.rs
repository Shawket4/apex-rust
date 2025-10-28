use actix_web::{HttpResponse, http::header};
use serde::Serialize;
use anyhow::Result;

pub fn msgpack_response<T: Serialize>(data: &T) -> Result<HttpResponse> {
    let bytes = rmp_serde::to_vec(data)?;
    Ok(HttpResponse::Ok()
        .insert_header((header::CONTENT_TYPE, "application/msgpack"))
        .body(bytes))
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
