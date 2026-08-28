use actix_cors::Cors;
use actix_web::{middleware, web, App, HttpServer};
use log::{error, info, warn};
use sqlx::postgres::PgPoolOptions;

use apex::auth::JwtAuth;
use apex::config::CONFIG;
use apex::handlers::*;
use apex::{api, boot, cutover, ingest, ops, parser};

/// Non-banksms routes: sessions and trip statistics. The legacy
/// /fleet-expenses stack is gone — the dashboard has called the banksms
/// routes exclusively since the migration, and the old handlers kept a
/// weaker (level 3) gate on financial data.
fn configure_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api/v1")
            .service(web::scope("/sessions").route(
                "/{id}/location-pings",
                web::get().to(get_session_location_pings).wrap(JwtAuth {
                    required_permission: Some(1),
                }),
            ))
            .route(
                "/trip-statistics",
                web::get().to(get_trip_statistics).wrap(JwtAuth {
                    required_permission: Some(3),
                }),
            )
            // Permission 4, not the 3 that opens statistics: this endpoint puts
            // a revenue figure against one driver's one trip, which is a
            // different disclosure from the same money shown in aggregate.
            .route(
                "/trips",
                web::get().to(apex::handlers::trips::get_trips).wrap(JwtAuth {
                    required_permission: Some(4),
                }),
            ),
    );
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // .env before the logger: RUST_LOG lives in .env.
    dotenv::dotenv().ok();
    env_logger::init();

    info!("Starting Apex Transport Rust Microservice");

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&CONFIG.database_url)
        .await
        .expect("Failed to connect to database");

    info!("Database connected");

    // ---- one-shot subcommands ------------------------------------------
    match std::env::args().nth(1).as_deref() {
        Some("cutover") => {
            info!("running cutover (run this with the HTTP service stopped)");
            match cutover::run(&pool).await {
                Ok(()) => {
                    info!("cutover complete");
                    return Ok(());
                }
                Err(e) => {
                    error!("cutover ABORTED, nothing was changed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("backfill") => {
            let wa = ingest::WhatsAppClient::from_config();
            match ingest::poller::backfill(&pool, &wa).await {
                Ok(n) => {
                    info!("backfill created {n} new transaction(s)");
                    return Ok(());
                }
                Err(e) => {
                    error!("backfill failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("reparse") => match ingest::poller::reparse_sweep(&pool).await {
            Ok((changed, created)) => {
                info!("reparse: {changed} status change(s), {created} new transaction(s)");
                return Ok(());
            }
            Err(e) => {
                error!("reparse failed: {e}");
                std::process::exit(1);
            }
        },
        _ => {}
    }

    // ---- boot ------------------------------------------------------------
    // A deploy can land before the cutover has run. In that state the new
    // tables don't exist and the OLD sqlx history occupies
    // banksms._sqlx_migrations — running the migrator would panic with
    // VersionMissing and crash-loop the whole service. Instead: banksms
    // routes answer 503, the poller stays off, and sessions/trip-statistics
    // keep serving until `apex-rust cutover` is run and the service restarted.
    let cutover_pending = boot::legacy_schema_present(&pool).await;

    if cutover_pending {
        error!(
            "legacy banksms schema detected — banksms is DISABLED until `apex-rust cutover` runs"
        );
        ops::notify::notify_ops("apex-rust deployed, cutover pending", "Bank-SMS module is serving 503s. Run `apex-rust cutover` (with the service stopped), then restart.").await;
    } else {
        boot::run_banksms_migrations(&pool)
            .await
            .expect("Failed to apply banksms migrations");

        match parser::templates::boot_check(&pool).await {
            Ok(broken) if broken.is_empty() => info!("template boot check: all samples pass"),
            Ok(broken) => {
                error!("templates DISABLED at boot (failed their own samples): {broken:?}");
                ops::notify::notify_ops(
                    "banksms template disabled at boot",
                    &format!(
                        "These templates failed their own samples and were disabled: {broken:?}"
                    ),
                )
                .await;
            }
            Err(e) => warn!("template boot check could not run: {e}"),
        }

        if CONFIG.ingest_enabled {
            let poll_pool = pool.clone();
            let poll_client = ingest::WhatsAppClient::from_config();
            tokio::spawn(async move {
                ingest::poller::run(poll_pool, poll_client).await;
            });
        } else {
            info!("ingest poller disabled by INGEST_ENABLED=false");
        }
    }

    let server_addr = format!("127.0.0.1:{}", CONFIG.server_port);
    info!("Starting HTTP server on http://{server_addr}");

    let wa_for_app = ingest::WhatsAppClient::from_config();

    HttpServer::new(move || {
        let cors = Cors::default()
            .allowed_methods(vec!["GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS"])
            .allowed_headers(vec![
                actix_web::http::header::AUTHORIZATION,
                actix_web::http::header::ACCEPT,
                actix_web::http::header::CONTENT_TYPE,
            ])
            .allowed_header("If-Match")
            .supports_credentials()
            .max_age(3600)
            .allow_any_origin();

        let app = App::new()
            .app_data(web::Data::new(pool.clone()))
            .wrap(cors)
            .wrap(middleware::Logger::default())
            .wrap(middleware::Compress::default())
            .app_data(web::Data::new(wa_for_app.clone()))
            .route("/health", web::get().to(health_check))
            .route("/healthz", web::get().to(api::health::healthz))
            .route("/readyz", web::get().to(api::health::readyz));

        // banksms routes FIRST: more specific prefixes, and actix matches the
        // first scope whose prefix matches.
        let app = if cutover_pending {
            app.configure(api::configure_cutover_pending)
        } else {
            app.configure(api::configure)
        };
        app.configure(configure_routes)
    })
    .workers(CONFIG.workers)
    .bind(&server_addr)?
    .run()
    .await
}
