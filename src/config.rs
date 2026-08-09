use once_cell::sync::Lazy;
use std::env;

pub struct Config {
    pub database_url: String,
    pub jwt_secret: String,
    pub server_host: String,
    pub server_port: u16,
    pub workers: usize,

    // --- bank-SMS ingestion -------------------------------------------------
    /// Base URL of the WhatsApp Go API. Localhost-only, unauthenticated.
    pub whatsapp_api_url: String,

    /// Reserved. The WhatsApp API has no auth today; carrying the field means
    /// adding one later is a config line rather than a refactor.
    pub whatsapp_api_token: Option<String>,

    /// The single chat whose messages we ingest. Never hardcode it: this is a
    /// 1:1 DM that also carries ordinary human conversation.
    pub target_chat_jid: String,

    pub poll_interval_secs: u64,

    /// How far back before the cursor each poll re-reads. WhatsApp delivers late
    /// in bursts after the phone reconnects, and the unique constraint on
    /// wa_message_id makes overlap free, so be generous.
    pub overlap_window_secs: i64,

    /// Ceiling for exponential backoff after WhatsApp API errors.
    pub poll_backoff_max_secs: u64,

    // --- notifications ------------------------------------------------------
    /// ntfy server. Defaults to the public instance; point it at a self-hosted
    /// one by setting NTFY_URL.
    pub ntfy_url: String,
    /// Topic to publish to. EMPTY DISABLES NOTIFICATIONS -- a deploy without
    /// this configured stays silent rather than erroring on every parse.
    pub ntfy_topic: String,
    pub ntfy_token: Option<String>,
    /// Above this many new transactions in one run, send a single summary
    /// instead of one push per row. Stops a backfill from firing 700 pushes.
    pub ntfy_max_individual: usize,
    /// Base URL used to build deep links, e.g.
    /// https://apextransport.ddns.net/fleet-expenses/224/edit
    pub dashboard_base_url: String,

    /// Shared secret the WhatsApp service sends with each webhook push.
    /// EMPTY DISABLES THE ENDPOINT: it is unauthenticated by necessity (the
    /// sender has no JWT), so without a secret it refuses every request rather
    /// than accepting anonymous writes.
    pub whatsapp_webhook_secret: Option<String>,

    /// Set false to run the HTTP API without the background poller (useful for
    /// tests, and for running a second instance that only serves reads).
    pub ingest_enabled: bool,
}

pub static CONFIG: Lazy<Config> = Lazy::new(|| {
    dotenv::dotenv().ok();

    Config {
        database_url: env::var("DATABASE_URL")
            .expect("DATABASE_URL must be set"),

        // Previously this fell back to the literal string "secret". That silently
        // turned a missing env var into a service that accepts forged admin
        // tokens, which is strictly worse than refusing to boot. FalconGo, which
        // issues these tokens, already hard-fails on the same condition.
        jwt_secret: env::var("JWT_SECRET")
            .expect("JWT_SECRET must be set — it must match FalconGo's signing secret"),

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

        whatsapp_api_url: env::var("WHATSAPP_API_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:3000".to_string()),
        whatsapp_api_token: env::var("WHATSAPP_API_TOKEN").ok().filter(|s| !s.is_empty()),

        target_chat_jid: env::var("TARGET_CHAT_JID").unwrap_or_default(),

        poll_interval_secs: env::var("POLL_INTERVAL_SECS")
            .unwrap_or_else(|_| "60".to_string())
            .parse()
            .expect("POLL_INTERVAL_SECS must be a valid number"),
        overlap_window_secs: env::var("OVERLAP_WINDOW_SECS")
            .unwrap_or_else(|_| "300".to_string())
            .parse()
            .expect("OVERLAP_WINDOW_SECS must be a valid number"),
        poll_backoff_max_secs: env::var("POLL_BACKOFF_MAX_SECS")
            .unwrap_or_else(|_| "900".to_string())
            .parse()
            .expect("POLL_BACKOFF_MAX_SECS must be a valid number"),

        whatsapp_webhook_secret: env::var("WHATSAPP_WEBHOOK_SECRET")
            .ok()
            .filter(|s| !s.is_empty()),

        ntfy_url: env::var("NTFY_URL")
            .unwrap_or_else(|_| "https://ntfy.sh".to_string()),
        ntfy_topic: env::var("NTFY_TOPIC").unwrap_or_default(),
        ntfy_token: env::var("NTFY_TOKEN").ok().filter(|s| !s.is_empty()),
        ntfy_max_individual: env::var("NTFY_MAX_INDIVIDUAL")
            .unwrap_or_else(|_| "5".to_string())
            .parse()
            .unwrap_or(5),
        dashboard_base_url: env::var("DASHBOARD_BASE_URL")
            .unwrap_or_else(|_| "https://apextransport.ddns.net".to_string()),

        ingest_enabled: env::var("INGEST_ENABLED")
            .map(|v| v != "false" && v != "0")
            .unwrap_or(true),
    }
});
