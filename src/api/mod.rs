//! Bank-SMS HTTP surface: 16 routes, everything permission 4, mounted under
//! /api/v1 ahead of the generic scope (actix matches the first scope whose
//! prefix matches).

pub mod health;
pub mod messages;
pub mod refdata;
pub mod registration;
pub mod templates_admin;
pub mod splits;
pub mod transactions;

use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};

use crate::auth::permissions::AuthContext;
use crate::auth::JwtAuth;
use crate::errors::{AppError, AppResult};

/// The verified caller, placed in request extensions by JwtAuth.
pub fn ctx(req: &HttpRequest) -> AppResult<AuthContext> {
    req.extensions()
        .get::<AuthContext>()
        .cloned()
        .ok_or(AppError::Unauthenticated)
}

fn admin() -> JwtAuth {
    JwtAuth {
        required_permission: Some(4),
    }
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api/v1/transactions")
            // Literal segments before the {id} matcher.
            .route(
                "/statistics",
                web::get().to(transactions::statistics).wrap(admin()),
            )
            .route("/export", web::get().to(transactions::export).wrap(admin()))
            .route("", web::get().to(transactions::list).wrap(admin()))
            .route("", web::post().to(transactions::create).wrap(admin()))
            .route("/{id}/split", web::get().to(splits::get).wrap(admin()))
            .route("/{id}/split", web::post().to(splits::split).wrap(admin()))
            .route("/{id}/split", web::put().to(splits::replace).wrap(admin()))
            .route("/{id}/unsplit", web::post().to(splits::unsplit).wrap(admin()))
            .route("/{id}", web::get().to(transactions::get).wrap(admin()))
            .route("/{id}", web::patch().to(transactions::patch).wrap(admin()))
            .route(
                "/{id}",
                web::delete().to(transactions::delete).wrap(admin()),
            ),
    )
    .service(
        web::scope("/api/v1/messages")
            .route("", web::get().to(messages::list).wrap(admin()))
            .route("/{id}", web::get().to(messages::get).wrap(admin())),
    )
    .service(
        web::scope("/api/v1/categories")
            .route("", web::get().to(refdata::list_categories).wrap(admin())),
    )
    .service(
        web::scope("/api/v1/parties")
            .route("/suggest", web::get().to(refdata::suggest_party).wrap(admin()))
            .route("", web::get().to(refdata::list_parties).wrap(admin())),
    )
    .service(
        web::scope("/api/v1/vehicles")
            .route("", web::get().to(refdata::list_vehicles).wrap(admin())),
    )
    .service(
        web::scope("/api/v1/templates")
            .route("", web::get().to(templates_admin::list).wrap(admin()))
            .route("", web::post().to(templates_admin::create).wrap(admin()))
            .route(
                "/{id}",
                web::patch().to(templates_admin::patch).wrap(admin()),
            ),
    );
}

/// Pre-cutover mode: the binary is deployed but `apex-rust cutover` has not
/// run yet, so the new tables don't exist. Every banksms route answers 503
/// with a reason instead of 500s from missing relations; the rest of the
/// service (sessions, trip statistics) keeps working.
pub fn configure_cutover_pending(cfg: &mut web::ServiceConfig) {
    async fn pending() -> HttpResponse {
        HttpResponse::ServiceUnavailable()
            .json(serde_json::json!({ "error": "cutover pending — run `apex-rust cutover`" }))
    }
    for prefix in [
        "/api/v1/transactions",
        "/api/v1/messages",
        "/api/v1/categories",
        "/api/v1/parties",
        "/api/v1/vehicles",
        "/api/v1/templates",
    ] {
        cfg.service(web::scope(prefix).default_service(web::route().to(pending)));
    }
}
