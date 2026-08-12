//! apex-rust as a library, so the integration suite in `tests/` can drive the
//! real modules (parser, ingest, cutover, API) against a real Postgres. The
//! binary in `main.rs` is a thin shell over this.

pub mod api;
pub mod auth;
pub mod boot;
pub mod config;
pub mod cutover;
pub mod db;
pub mod errors;
pub mod handlers;
pub mod ingest;
pub mod models;
pub mod ops;
pub mod parser;
pub mod utils;
