//! Liveness and readiness. `/health` is what the deploy pipeline curls;
//! `/readyz` additionally proves the database and the WhatsApp API answer.

use actix_web::{web, HttpResponse};
use sqlx::PgPool;

use crate::ingest::WhatsAppClient;

pub async fn healthz() -> HttpResponse {
    HttpResponse::Ok().body("ok")
}

pub async fn readyz(pool: web::Data<PgPool>, wa: web::Data<WhatsAppClient>) -> HttpResponse {
    if let Err(e) = sqlx::query("SELECT 1").execute(pool.get_ref()).await {
        return HttpResponse::ServiceUnavailable().body(format!("db: {e}"));
    }
    if let Err(e) = wa.ping().await {
        return HttpResponse::ServiceUnavailable().body(format!("whatsapp: {e}"));
    }
    HttpResponse::Ok().body("ready")
}
