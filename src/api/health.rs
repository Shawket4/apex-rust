//! Liveness and readiness. `/health` is what the deploy pipeline curls;
//! `/readyz` additionally proves the database and the WhatsApp API answer.

use actix_web::{web, HttpResponse};
use sqlx::PgPool;

use crate::ingest::WhatsAppClient;

pub async fn healthz() -> HttpResponse {
    HttpResponse::Ok().body("ok")
}

pub async fn readyz(pool: web::Data<PgPool>, wa: web::Data<WhatsAppClient>) -> HttpResponse {
    // `/readyz` is unauthenticated — the deploy pipeline curls it before any
    // token exists. It gets the status code and which dependency is down,
    // which is all it can act on; the error itself goes to the operator log.
    // A sqlx error names the host, database and role it failed to reach, and
    // that is not something to hand to anyone who can reach the port.
    if let Err(e) = sqlx::query("SELECT 1").execute(pool.get_ref()).await {
        log::error!("readyz: database probe failed: {e}");
        return HttpResponse::ServiceUnavailable().body("db");
    }
    if let Err(e) = wa.ping().await {
        log::error!("readyz: whatsapp probe failed: {e}");
        return HttpResponse::ServiceUnavailable().body("whatsapp");
    }
    HttpResponse::Ok().body("ready")
}
