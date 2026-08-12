//! Ingestion: store raw first, parse second, never lose a message.
//!
//! One code path — `poller::process_batch` — shared by the poller, the
//! backfill CLI and the reparse sweep. No webhook: the 60s poller is both the
//! guarantee and, at this traffic (3-8 bank SMS/day), the latency floor.

pub mod cursor;
pub mod poller;
pub mod whatsapp_client;

pub use whatsapp_client::WhatsAppClient;
