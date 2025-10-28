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

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // Load environment variables
    dotenv::dotenv().ok();
    
    let host = env::var("SERVER_HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let port = env::var("SERVER_PORT").unwrap_or_else(|_| "3002".to_string());
    
    // SSL certificate paths
    let ssl_cert = env::var("SSL_CERT_PATH")
        .unwrap_or_else(|_| "/etc/letsencrypt/live/apextransport.ddns.net/fullchain.pem".to_string());
    let ssl_key = env::var("SSL_KEY_PATH")
        .unwrap_or_else(|_| "/etc/letsencrypt/live/apextransport.ddns.net/privkey.pem".to_string());
    
    // Get allowed origins from environment
    let cors_origins = env::var("CORS_ALLOWED_ORIGINS")
        .unwrap_or_else(|_| "https://apextransport.ddns.net".to_string());
    
    println!("🚀 Starting HTTPS server at {}:{}", host, port);
    println!("🔒 Using SSL cert: {}", ssl_cert);
    println!("🌐 CORS origins: {}", cors_origins);

    // Configure SSL
    let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    builder
        .set_private_key_file(&ssl_key, SslFiletype::PEM)
        .unwrap();
    builder.set_certificate_chain_file(&ssl_cert).unwrap();

    HttpServer::new(move || {
        // Configure CORS
        let mut cors = Cors::default()
            .allowed_methods(vec!["GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS"])
            .allowed_headers(vec![
                actix_web::http::header::AUTHORIZATION,
                actix_web::http::header::ACCEPT,
                actix_web::http::header::CONTENT_TYPE,
            ])
            .supports_credentials()
            .max_age(3600);

        // Add allowed origins from environment variable
        for origin in cors_origins.split(',') {
            let origin = origin.trim();
            if !origin.is_empty() {
                cors = cors.allowed_origin(origin);
            }
        }

        App::new()
            .wrap(cors)
            .wrap(middleware::Logger::default())
            .wrap(middleware::Compress::default())
            // Your routes here
            .route("/health", web::get().to(|| async { "OK" }))
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
    .bind_openssl(format!("{}:{}", host, port), builder)?
    .run()
    .await
}