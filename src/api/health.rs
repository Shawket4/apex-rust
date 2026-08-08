//! Health probes. No auth: these sit in front of the JWT middleware so a probe
//! never depends on FalconGo being reachable.

use actix_web::{web, HttpResponse};
use serde::Serialize;
use sqlx::PgPool;

use crate::ingest::WhatsAppClient;

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

/// Liveness: is the process up? Deliberately checks nothing else -- a liveness
/// probe that fails on a dependency outage causes a restart loop that makes the
/// outage worse.
pub async fn healthz() -> HttpResponse {
    HttpResponse::Ok().json(Health { status: "ok" })
}

#[derive(Serialize)]
struct Readiness {
    status: &'static str,
    database: bool,
    whatsapp_api: bool,
}

/// Readiness: can this instance actually serve? Checks the database and the
/// WhatsApp API. Returns 503 when either is down so a load balancer stops
/// sending traffic, while liveness keeps the process alive.
pub async fn readyz(pool: web::Data<PgPool>, wa: web::Data<WhatsAppClient>) -> HttpResponse {
    let database = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(pool.get_ref())
        .await
        .is_ok();

    let whatsapp_api = wa.ping().await.is_ok();

    let body = Readiness {
        status: if database && whatsapp_api { "ready" } else { "degraded" },
        database,
        whatsapp_api,
    };

    if database && whatsapp_api {
        HttpResponse::Ok().json(body)
    } else {
        HttpResponse::ServiceUnavailable().json(body)
    }
}
