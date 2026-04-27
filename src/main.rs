mod config;
mod auth;
mod models;
mod handlers;
mod db;
mod utils;

use actix_web::{web, App, HttpServer, middleware};
use actix_cors::Cors;
use sqlx::postgres::PgPoolOptions;
use log::info;

use crate::config::CONFIG;
use crate::auth::JwtAuth;
use crate::handlers::*;

fn configure_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api/v1")
            // Session Routes
            .service(
                web::scope("/sessions")
                    .route(
                        "/{id}/location-pings",
                        web::get()
                            .to(get_session_location_pings)
                            .wrap(JwtAuth { required_permission: Some(1) })
                    )
            )
            // Trip Statistics Routes
            .route(
                "/trip-statistics",
                web::get()
                    .to(get_trip_statistics)
                    .wrap(JwtAuth { required_permission: Some(3) })
            )
            // Fleet Expenses Routes
            .route(
                "/fleet-expenses",
                web::post()
                    .to(create_expense_handler)
                    .wrap(JwtAuth { required_permission: Some(4) })
            )
            .route(
                "/fleet-expenses",
                web::get()
                    .to(list_unified_expenses_handler)
                    .wrap(JwtAuth { required_permission: Some(4) })
            )
            .route(
                "/fleet-expenses/statistics",
                web::get()
                    .to(get_unified_expense_statistics_handler)
                    .wrap(JwtAuth { required_permission: Some(4) })
            )
            .route("/fleet-expenses/export", web::get().to(export_expenses_handler).wrap(JwtAuth { required_permission: Some(4) }))
            .route(
                "/fleet-expenses/{id}",
                web::get()
                    .to(get_expense_handler)
                    .wrap(JwtAuth { required_permission: Some(4) })
            )
            .route(
                "/fleet-expenses/{id}",
                web::put()
                    .to(update_expense_handler)
                    .wrap(JwtAuth { required_permission: Some(4) })
            )
            .route(
                "/fleet-expenses/{id}",
                web::delete()
                    .to(delete_expense_handler)
                    .wrap(JwtAuth { required_permission: Some(4) })
            )
    );
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init();

    info!("Starting Apex Transport Rust Microservice");
    info!("Connecting to database...");

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&CONFIG.database_url)
        .await
        .expect("Failed to connect to database");

    info!("Database connected successfully");

    // Bind to loopback only — nginx terminates TLS and proxies to us over HTTP.
    // The service is unreachable from outside the machine at the OS level.
    let server_addr = format!("127.0.0.1:{}", CONFIG.server_port);

    info!("Starting HTTP server on http://{}", server_addr);

    HttpServer::new(move || {
        // CORS is largely irrelevant here since all requests arrive via nginx,
        // but kept permissive for local development convenience.
        let cors = Cors::default()
            .allowed_methods(vec!["GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS"])
            .allowed_headers(vec![
                actix_web::http::header::AUTHORIZATION,
                actix_web::http::header::ACCEPT,
                actix_web::http::header::CONTENT_TYPE,
            ])
            .supports_credentials()
            .max_age(3600)
            .allow_any_origin();

        App::new()
            .app_data(web::Data::new(pool.clone()))
            .wrap(cors)
            .wrap(middleware::Logger::default())
            .wrap(middleware::Compress::default())
            .route("/health", web::get().to(health_check))
            .configure(configure_routes)
    })
    .workers(CONFIG.workers)
    .bind(&server_addr)?
    .run()
    .await
}