use actix_cors::Cors;
use actix_web::{middleware, web, App, HttpServer};
use log::{error, info, warn};
use sqlx::postgres::PgPoolOptions;

use apex::auth::JwtAuth;
use apex::config::CONFIG;
use apex::handlers::*;
use apex::{api, boot, cutover, ingest, ops, parser};

/// Non-banksms routes: sessions, trip statistics and the trips list.
///
/// The scope itself is built in the lib so the integration suite mounts the
/// same wiring the binary serves. The legacy /fleet-expenses stack is gone --
/// the dashboard has called the banksms routes exclusively since the
/// migration, and the old handlers kept a weaker (level 3) gate on financial
/// data.
fn configure_routes(cfg: &mut web::ServiceConfig) {
    apex::handlers::configure_api_v1(cfg);
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // .env before the logger: RUST_LOG lives in .env.
    dotenv::dotenv().ok();

    // tracing_subscriber rather than env_logger, for one reason: it lets the
    // Sentry layer see events. This crate logs through `log`, and
    // tracing-subscriber's tracing-log bridge picks those up, so every existing
    // log::info!/error! still prints exactly as before and RUST_LOG is still
    // what controls it.
    //
    // The payoff is sqlx: it emits a span per query with elapsed time, so an
    // error event now arrives carrying the last queries the request ran and how
    // long each took. Sentry gets its own filter so it can see sqlx at debug
    // without that going to the console.
    {
        use tracing_subscriber::prelude::*;
        let console = tracing_subscriber::fmt::layer().with_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        );
        let sentry_layer = sentry_tracing::layer().with_filter(
            tracing_subscriber::EnvFilter::try_from_env("SENTRY_TRACING_FILTER")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,sqlx=debug")),
        );
        tracing_subscriber::registry()
            .with(sentry_layer)
            .with(console)
            .init();
    }

    // Sentry, if SENTRY_DSN is set. The guard has to outlive the server, so it
    // is bound here rather than in a helper: dropping it flushes the queue and
    // shuts the transport down. With no DSN this is None and nothing changes.
    let _sentry = apex::observability::init();

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
            // Captures errors that resolve to 5xx, and attaches request
            // context. It does NOT report 4xx: a client sending a bad request
            // is not a bug in this service, and the volume would bury the ones
            // that are. Everything it attaches passes through the scrubber in
            // `observability` before it leaves the process.
            .wrap(
                sentry_actix::Sentry::builder()
                    .capture_server_errors(true)
                    // Transactions, so request throughput and latency are
                    // visible and not just failures.
                    .start_transaction(true)
                    .finish(),
            )
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
