mod config;
mod api;
mod auth;
mod errors;
mod ingest;
mod ops;
mod parser;
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

/// Apply the `banksms` migrations at boot, keeping their bookkeeping isolated
/// from apex-petroapp's.
///
/// The problem: apex-petroapp owns `public._sqlx_migrations` in *this same
/// database* (its versions 1..3). If this service used the default table too,
/// each migrator would see the other's rows as unknown versions and fail with
/// `VersionMissing`.
///
/// sqlx 0.8 has no `set_migration_table` — the name `_sqlx_migrations` is
/// hardcoded and created *unqualified*, so it lands in the first existing schema
/// on `search_path`. So: create the schema, then run the migrator over a single
/// dedicated connection whose `search_path` starts with `banksms`. The bookkeeping
/// table becomes `banksms._sqlx_migrations` and petroapp's is left untouched.
///
/// Scoping this to one connection rather than the pool matters: a pool-wide
/// `search_path` change would silently reorder name resolution for every existing
/// query in this service, so any future `public` table sharing a name with a
/// `banksms` table would be shadowed. Every statement in the migrations is
/// schema-qualified, so nothing else depends on this setting.
async fn run_banksms_migrations(pool: &sqlx::PgPool) {
    use sqlx::Executor;

    // Must exist before `search_path` can place the bookkeeping table in it:
    // Postgres silently ignores non-existent schemas in search_path, which would
    // put `_sqlx_migrations` back in `public`. Migration 1 repeats this
    // idempotently, which is fine.
    sqlx::query("CREATE SCHEMA IF NOT EXISTS banksms")
        .execute(pool)
        .await
        .expect("Failed to create banksms schema");

    let mut conn = pool
        .acquire()
        .await
        .expect("Failed to acquire migration connection");

    conn.execute("SET search_path TO banksms, public")
        .await
        .expect("Failed to set search_path for migrations");

    sqlx::migrate!("./migrations")
        .run(&mut *conn)
        .await
        .expect("Failed to apply banksms migrations");

    info!("Migrations applied (banksms._sqlx_migrations)");
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

    run_banksms_migrations(&pool).await;

    let wa_client = ingest::WhatsAppClient::from_config();

    // One-shot backfill: `apex-rust backfill`. Walks chat history through the same
    // insert path the poller uses, then exits without starting the HTTP server.
    if std::env::args().nth(1).as_deref() == Some("backfill") {
        match ingest::poller::backfill(&pool, &wa_client).await {
            Ok(n) => {
                info!("backfill inserted {n} new messages");
                return Ok(());
            }
            Err(e) => {
                log::error!("backfill failed: {e}");
                std::process::exit(1);
            }
        }
    }

    // The poller is an independent task: ingestion never blocks a request, and a
    // slow request never delays a poll.
    if CONFIG.ingest_enabled {
        let poll_pool = pool.clone();
        let poll_client = wa_client.clone();
        tokio::spawn(async move {
            ingest::poller::run(poll_pool, poll_client).await;
        });
    } else {
        info!("ingest poller disabled by INGEST_ENABLED=false");
    }

    // Bind to loopback only — nginx terminates TLS and proxies to us over HTTP.
    // The service is unreachable from outside the machine at the OS level.
    let server_addr = format!("127.0.0.1:{}", CONFIG.server_port);

    info!("Starting HTTP server on http://{}", server_addr);

    let wa_for_app = wa_client.clone();

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
            .app_data(web::Data::new(wa_for_app.clone()))
            .route("/health", web::get().to(health_check))
            .route("/healthz", web::get().to(api::health::healthz))
            .route("/readyz", web::get().to(api::health::readyz))
            .route("/metrics", web::get().to(api::health::metrics))
            // Bank-SMS routes FIRST: their prefixes are more specific, and
            // actix matches the first scope whose prefix matches rather than
            // falling through, so the existing generic /api/v1 scope would
            // otherwise shadow every one of them.
            .configure(api::configure)
            .configure(configure_routes)
    })
    .workers(CONFIG.workers)
    .bind(&server_addr)?
    .run()
    .await
}