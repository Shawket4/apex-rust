//! Test harness: real Postgres databases (one per test), the minimal
//! FalconGo-owned public tables, and a process-global mock of the WhatsApp Go
//! API that serves the vendored production corpus.

use actix_web::{web, App, HttpResponse, HttpServer};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::sync::{Arc, Mutex, Once, OnceLock};

pub const TARGET_JID: &str = "201280701070@s.whatsapp.net";
pub const JWT_SECRET: &str = "test-secret";

fn pg_base() -> String {
    std::env::var("TEST_PG_BASE").unwrap_or_else(|_| "postgres://127.0.0.1:5432".to_string())
}

/// One corpus message as vendored in tests/fixtures/wa_corpus.json.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct CorpusMessage {
    pub id: String,
    pub chat_jid: String,
    #[serde(default)]
    pub sender_jid: Option<String>,
    #[serde(default)]
    pub content: String,
    pub timestamp: DateTime<Utc>,
    #[serde(default)]
    pub is_from_me: bool,
    #[serde(default)]
    pub media_type: Option<String>,
}

pub fn load_corpus() -> Vec<CorpusMessage> {
    let raw = include_str!("../fixtures/wa_corpus.json");
    let mut msgs: Vec<CorpusMessage> = serde_json::from_str(raw).expect("corpus parses");
    // The real API serves newest-first.
    msgs.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then(b.id.cmp(&a.id)));
    msgs
}

#[derive(Debug, Clone, Deserialize)]
pub struct OracleRow {
    pub wa_message_id: String,
    pub template: Option<String>,
    pub direction: Option<String>,
    pub amount: Option<String>,
    pub currency: Option<String>,
    pub account: Option<String>,
    pub counterparty: Option<String>,
    pub reference: Option<String>,
    pub occurred_at: Option<DateTime<Utc>>,
    pub old_txn_id: i64,
    pub soft_deleted: bool,
}

pub fn load_oracle() -> Vec<OracleRow> {
    serde_json::from_str(include_str!("../fixtures/parse_oracle.json")).expect("oracle parses")
}

/* ------------------------------ mock API -------------------------------- */

#[derive(Default)]
pub struct MockState {
    pub messages: Vec<CorpusMessage>,
    /// Serve this many pages, then answer 500 — the kill-mid-batch lever.
    pub fail_after_pages: Option<u32>,
}

pub struct Mock {
    pub state: Arc<Mutex<MockState>>,
    /// Tests that reprogram the mock hold this to serialize against each other.
    pub guard: Mutex<()>,
}

static MOCK: OnceLock<Mock> = OnceLock::new();
static INIT: Once = Once::new();

#[derive(Deserialize)]
struct PageQuery {
    limit: Option<usize>,
    offset: Option<usize>,
    start_time: Option<DateTime<Utc>>,
}

async fn mock_messages(
    state: web::Data<Arc<Mutex<MockState>>>,
    query: web::Query<PageQuery>,
) -> HttpResponse {
    let limit = query.limit.unwrap_or(100).min(100);
    let offset = query.offset.unwrap_or(0);
    let s = state.lock().unwrap();

    if let Some(max_pages) = s.fail_after_pages {
        if (offset / limit.max(1)) as u32 >= max_pages {
            return HttpResponse::InternalServerError().body("mock: simulated crash");
        }
    }

    let filtered: Vec<&CorpusMessage> = s
        .messages
        .iter()
        .filter(|m| query.start_time.map_or(true, |st| m.timestamp >= st))
        .collect();
    let page: Vec<&CorpusMessage> = filtered.into_iter().skip(offset).take(limit).collect();

    HttpResponse::Ok().json(serde_json::json!({
        "code": "SUCCESS",
        "message": "Success get chat messages",
        "results": { "data": page, "pagination": { "limit": limit, "offset": offset, "total": s.messages.len() } }
    }))
}

async fn mock_chats() -> HttpResponse {
    HttpResponse::Ok().json(serde_json::json!({ "code": "SUCCESS", "results": { "data": [] } }))
}

/// Start the mock (once per process) on a dedicated thread with its own
/// runtime, set every env var the lazy CONFIG will read, and return the mock
/// handle. MUST be called before anything touches CONFIG.
pub fn init() -> &'static Mock {
    INIT.call_once(|| {
        let state = Arc::new(Mutex::new(MockState::default()));
        let (tx, rx) = std::sync::mpsc::channel::<u16>();
        let thread_state = state.clone();

        std::thread::spawn(move || {
            actix_web::rt::System::new().block_on(async move {
                let data = web::Data::new(thread_state);
                let server = HttpServer::new(move || {
                    App::new()
                        .app_data(data.clone())
                        .route("/chat/{jid}/messages", web::get().to(mock_messages))
                        .route("/chats", web::get().to(mock_chats))
                })
                .bind(("127.0.0.1", 0))
                .expect("mock bind");
                let port = server.addrs()[0].port();
                tx.send(port).unwrap();
                server.run().await.unwrap();
            });
        });

        let port = rx.recv().expect("mock port");

        std::env::set_var("WHATSAPP_API_URL", format!("http://127.0.0.1:{port}"));
        std::env::set_var("TARGET_CHAT_JID", TARGET_JID);
        std::env::set_var("JWT_SECRET", JWT_SECRET);
        std::env::set_var("DATABASE_URL", format!("{}/postgres", pg_base()));
        std::env::set_var("OVERLAP_WINDOW_SECS", "300");
        std::env::set_var("POLL_INTERVAL_SECS", "60");
        std::env::set_var("NTFY_TOPIC", ""); // notifications off in tests
        std::env::set_var("CUTOVER_SKIP_FLOORS", "1"); // rehearsal fixtures, not prod data

        MOCK.set(Mock {
            state,
            guard: Mutex::new(()),
        })
        .unwrap_or_else(|_| panic!("mock already set"));
    });
    MOCK.get().unwrap()
}

/* ------------------------------ databases ------------------------------- */

/// Create a fresh database and the minimal FalconGo-owned tables the module
/// reads (and, for the cutover rehearsal, the legacy source tables).
pub async fn fresh_db(name: &str) -> PgPool {
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&format!("{}/postgres", pg_base()))
        .await
        .expect("admin connect — is local Postgres running?");
    sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
        .execute(&admin)
        .await
        .expect("drop db");
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin)
        .await
        .expect("create db");

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&format!("{}/{name}", pg_base()))
        .await
        .expect("test db connect");

    sqlx::raw_sql(
        r#"
        CREATE TABLE public.cars (
            id BIGSERIAL PRIMARY KEY, car_no_plate TEXT,
            created_at TIMESTAMPTZ DEFAULT now(), updated_at TIMESTAMPTZ DEFAULT now(),
            deleted_at TIMESTAMPTZ
        );
        CREATE TABLE public.drivers (
            id BIGSERIAL PRIMARY KEY, name TEXT, mobile_number TEXT,
            created_at TIMESTAMPTZ DEFAULT now(), updated_at TIMESTAMPTZ DEFAULT now(),
            deleted_at TIMESTAMPTZ
        );
        CREATE TABLE public.employees (
            id BIGSERIAL PRIMARY KEY, name TEXT, mobile_number TEXT,
            is_active BOOLEAN DEFAULT TRUE,
            created_at TIMESTAMPTZ DEFAULT now(), updated_at TIMESTAMPTZ DEFAULT now(),
            deleted_at TIMESTAMPTZ
        );
        -- loans.id is INT4 in production (GORM), mirrored here on purpose.
        CREATE TABLE public.loans (
            id SERIAL PRIMARY KEY,
            created_at TIMESTAMPTZ DEFAULT now(), updated_at TIMESTAMPTZ DEFAULT now(),
            deleted_at TIMESTAMPTZ,
            driver_id INTEGER, employee_id INTEGER,
            amount DOUBLE PRECISION, method TEXT, date TEXT,
            is_paid BOOLEAN NOT NULL DEFAULT FALSE,
            kind VARCHAR(16) NOT NULL DEFAULT 'advance',
            description TEXT
        );
        CREATE TABLE public.fuel_events (
            id BIGSERIAL PRIMARY KEY,
            created_at TIMESTAMPTZ DEFAULT now(), updated_at TIMESTAMPTZ DEFAULT now(),
            deleted_at TIMESTAMPTZ,
            date TEXT, driver_name TEXT, car_no_plate TEXT, transporter TEXT,
            liters NUMERIC, price_per_liter NUMERIC, price NUMERIC, method TEXT
        );
        CREATE TABLE public.fleet_expenses (
            id SERIAL PRIMARY KEY, car_no_plate VARCHAR(50), expense_date DATE,
            expense_type VARCHAR(100), amount NUMERIC(12,2), description TEXT,
            company VARCHAR(100), paid_by VARCHAR(255), payment_method VARCHAR(50),
            created_by INTEGER, created_at TIMESTAMPTZ DEFAULT now(),
            updated_at TIMESTAMPTZ DEFAULT now(), deleted_at TIMESTAMPTZ
        );
        "#,
    )
    .execute(&pool)
    .await
    .expect("public tables");

    pool
}

/// Apply the archived legacy migrations (the pre-rebuild schema) — the
/// cutover rehearsal's starting state.
pub async fn apply_legacy_schema(pool: &PgPool) {
    let mut entries: Vec<_> = std::fs::read_dir(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/legacy_migrations"
    ))
    .expect("legacy_migrations dir")
    .filter_map(|e| e.ok())
    .map(|e| e.path())
    .filter(|p| p.to_string_lossy().ends_with(".up.sql"))
    .collect();
    entries.sort();

    use sqlx::Executor;
    let mut conn = pool.acquire().await.expect("conn");
    // The legacy migrations were applied through a banksms-first search_path.
    conn.execute("SET search_path TO banksms, public")
        .await
        .ok();
    for path in entries {
        let sql = std::fs::read_to_string(&path).expect("read legacy migration");
        conn.execute(sqlx::raw_sql(&sql))
            .await
            .unwrap_or_else(|e| panic!("legacy migration {path:?} failed: {e}"));
    }
}

/// Forge an admin JWT the way FalconGo issues them (HS256, user_id number,
/// iss = the id as a string, no sub).
pub fn admin_token(user_id: i64) -> String {
    use jsonwebtoken::{encode, EncodingKey, Header};
    #[derive(serde::Serialize)]
    struct FalconGoClaims {
        user_type: String,
        user_id: i64,
        driver_id: i64,
        permission: i32,
        exp: i64,
        iss: String,
    }
    encode(
        &Header::default(), // HS256
        &FalconGoClaims {
            user_type: "admin_user".into(),
            user_id,
            driver_id: 0,
            permission: 4,
            exp: (Utc::now() + chrono::Duration::hours(1)).timestamp(),
            iss: user_id.to_string(),
        },
        &EncodingKey::from_secret(JWT_SECRET.as_bytes()),
    )
    .unwrap()
}
