use once_cell::sync::Lazy;
use std::env;

pub struct Config {
    pub database_url: String,
    pub jwt_secret: String,
    pub server_host: String,
    pub server_port: u16,
    pub workers: usize,
}

pub static CONFIG: Lazy<Config> = Lazy::new(|| {
    dotenv::dotenv().ok();
    
    Config {
        database_url: env::var("DATABASE_URL")
            .expect("DATABASE_URL must be set"),
        jwt_secret: env::var("JWT_SECRET")
            .unwrap_or_else(|_| "secret".to_string()),
        server_host: env::var("SERVER_HOST")
            .unwrap_or_else(|_| "127.0.0.1".to_string()),
        server_port: env::var("SERVER_PORT")
            .unwrap_or_else(|_| "8080".to_string())
            .parse()
            .expect("SERVER_PORT must be a valid number"),
        workers: env::var("WORKERS")
            .unwrap_or_else(|_| "4".to_string())
            .parse()
            .unwrap_or(4),
    }
});
