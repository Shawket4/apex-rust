mod config;
mod auth;
mod models;
mod handlers;
mod db;
mod utils;

use std::env;
use actix_web::{web, App, HttpServer, middleware};
use openssl::ssl::{SslAcceptor, SslFiletype, SslMethod};
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
    
    let ssl_cert = env::var("SSL_CERT_PATH")
        .unwrap_or_else(|_| "/etc/letsencrypt/live/apextransport.ddns.net/fullchain.pem".to_string());
    let ssl_key = env::var("SSL_KEY_PATH")
        .unwrap_or_else(|_| "/etc/letsencrypt/live/apextransport.ddns.net/privkey.pem".to_string());
    
    let cors_origins = env::var("CORS_ALLOWED_ORIGINS")
        .unwrap_or_else(|_| "http://localhost:3000".to_string());
    
    let server_addr = format!("{}:{}", CONFIG.server_host, CONFIG.server_port);
    
    info!("CORS origins: {}", cors_origins);
    
    let ssl_available = std::path::Path::new(&ssl_cert).exists() 
        && std::path::Path::new(&ssl_key).exists();
    
    let create_app = move || {
        let mut cors = Cors::default()
            .allowed_methods(vec!["GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS"])
            .allowed_headers(vec![
                actix_web::http::header::AUTHORIZATION,
                actix_web::http::header::ACCEPT,
                actix_web::http::header::CONTENT_TYPE,
            ])
            .supports_credentials()
            .max_age(3600);
        
        for origin in cors_origins.split(',') {
            let origin = origin.trim();
            if !origin.is_empty() {
                cors = cors.allowed_origin(origin);
            }
        }
        
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .wrap(cors)
            .wrap(middleware::Logger::default())
            .wrap(middleware::Compress::default())
            .route("/health", web::get().to(health_check))
            .configure(configure_routes)
    };

    if ssl_available {
        info!("SSL certificates found");
        info!("Starting HTTPS server on https://{}", server_addr);
        
        let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls())
            .expect("Failed to create SSL acceptor");
        builder.set_private_key_file(&ssl_key, SslFiletype::PEM)
            .expect("Failed to set private key");
        builder.set_certificate_chain_file(&ssl_cert)
            .expect("Failed to set certificate chain");
        
        HttpServer::new(create_app)
            .workers(CONFIG.workers)
            .bind_openssl(&server_addr, builder)?
            .run()
            .await
    } else {
        info!("SSL certificates not found - starting HTTP server instead");
        info!("Starting HTTP server on http://{}", server_addr);
        
        HttpServer::new(create_app)
            .workers(CONFIG.workers)
            .bind(&server_addr)?
            .run()
            .await
    }
}