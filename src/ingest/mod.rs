//! Bank-SMS ingestion: pull messages from the WhatsApp Go API and store them
//! verbatim. Store raw first, parse second -- parsing must never be able to block
//! or drop an ingest.

pub mod cursor;
pub mod poller;
pub mod whatsapp_client;

pub use whatsapp_client::WhatsAppClient;
