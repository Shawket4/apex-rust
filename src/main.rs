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

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init();

    info!("Starting Trip Statistics Rust Microservice");
    info!("Connecting to database...");

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&CONFIG.database_url)
        .await
        .expect("Failed to connect to database");

    info!("Database connected successfully");

    let server_addr = format!("{}:{}", CONFIG.server_host, CONFIG.server_port);
    info!("Starting server on http://{}", server_addr);

    HttpServer::new(move || {
        let cors = Cors::default()
            .allow_any_origin()
            .allow_any_method()
            .allow_any_header()
            .max_age(3600);

        App::new()
            .app_data(web::Data::new(pool.clone()))
            .wrap(cors)
            .wrap(middleware::Logger::default())
            .wrap(middleware::Compress::default())
            .route("/health", web::get().to(health_check))
            .service(
                web::scope("/api/v1")
                    .route(
                        "/trip-statistics",
                        web::get()
                            .to(get_trip_statistics)
                            .wrap(JwtAuth { required_permission: Some(3) })
                    )
            )
    })
    .workers(CONFIG.workers)
    .bind(&server_addr)?
    .run()
    .await
}
