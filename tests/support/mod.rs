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
            id SERIAL PRIMARY KEY, car_no_plate TEXT,
            -- Empty string, not NULL, on untracked vehicles — mirroring what
            -- production actually stores; the dashboard normalises it.
            etit_car_id TEXT DEFAULT '',
            created_at TIMESTAMPTZ DEFAULT now(), updated_at TIMESTAMPTZ DEFAULT now(),
            deleted_at TIMESTAMPTZ
        );
        CREATE TABLE public.drivers (
            id SERIAL PRIMARY KEY, name TEXT, mobile_number TEXT,
            created_at TIMESTAMPTZ DEFAULT now(), updated_at TIMESTAMPTZ DEFAULT now(),
            deleted_at TIMESTAMPTZ
        );
        CREATE TABLE public.employees (
            id BIGSERIAL PRIMARY KEY, name TEXT, mobile_number TEXT,
            is_active BOOLEAN DEFAULT TRUE,
            created_at TIMESTAMPTZ DEFAULT now(), updated_at TIMESTAMPTZ DEFAULT now(),
            deleted_at TIMESTAMPTZ
        );
        -- Every GORM-owned id is INT4 in production except employees.id, which
        -- is INT8. Mirrored exactly: reading an INT4 column as i64 (or the
        -- reverse) is a decode error at runtime, not a compile error, and it
        -- has already panicked a worker mid-request once.
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
            id SERIAL PRIMARY KEY,
            created_at TIMESTAMPTZ DEFAULT now(), updated_at TIMESTAMPTZ DEFAULT now(),
            deleted_at TIMESTAMPTZ,
            date TEXT, driver_name TEXT, car_no_plate TEXT, transporter TEXT,
            liters NUMERIC, price_per_liter NUMERIC, price NUMERIC, method TEXT,
            petroapp_bill_id BIGINT
        );
        CREATE TABLE public.fleet_expenses (
            id SERIAL PRIMARY KEY, car_no_plate VARCHAR(50), expense_date DATE,
            expense_type VARCHAR(100), amount NUMERIC(12,2), description TEXT,
            company VARCHAR(100), paid_by VARCHAR(255), payment_method VARCHAR(50),
            created_by INTEGER, created_at TIMESTAMPTZ DEFAULT now(),
            updated_at TIMESTAMPTZ DEFAULT now(), deleted_at TIMESTAMPTZ
        );
        -- Trips and their fee mappings are FalconGo-owned. Column types mirror
        -- production exactly: `date` really is TEXT ('YYYY-MM-DD'), `id` really
        -- is INT4, and both `distance` and `fee` are NUMERIC -- the revenue SQL
        -- casts them, and a mismatch here would hide a cast bug.
        CREATE TABLE public.trips (
            id SERIAL PRIMARY KEY,
            created_at TIMESTAMP DEFAULT now(), updated_at TIMESTAMP DEFAULT now(),
            deleted_at TIMESTAMP,
            car_id BIGINT, driver_id BIGINT,
            car_no_plate TEXT, driver_name TEXT, transporter TEXT,
            tank_capacity BIGINT,
            company TEXT, terminal TEXT, drop_off_point TEXT, location_name TEXT,
            capacity BIGINT, gas_type TEXT,
            date TEXT,
            revenue NUMERIC, mileage NUMERIC,
            receipt_no TEXT, parent_trip_id BIGINT
        );
        CREATE TABLE public.fee_mappings (
            id SERIAL PRIMARY KEY,
            created_at TIMESTAMP DEFAULT now(), updated_at TIMESTAMP DEFAULT now(),
            deleted_at TIMESTAMP,
            company TEXT, terminal TEXT, drop_off_point TEXT,
            distance NUMERIC, fee NUMERIC,
            latitude NUMERIC, longitude NUMERIC,
            osrm_distance NUMERIC, osrm_duration NUMERIC
        );
        -- The nested graph a trips-list row carries: its receipt steps, and for
        -- a container, its parent header with that parent's scanned receipts.
        CREATE TABLE public.parent_trips (
            id SERIAL PRIMARY KEY,
            created_at TIMESTAMP DEFAULT now(), updated_at TIMESTAMP DEFAULT now(),
            deleted_at TIMESTAMP,
            car_id BIGINT, driver_id BIGINT, car_no_plate TEXT, driver_name TEXT,
            transporter TEXT, tank_capacity BIGINT,
            company TEXT, terminal TEXT, date TEXT,
            session_id BIGINT, author TEXT, overwriter TEXT
        );
        CREATE TABLE public.receipt_steps (
            id SERIAL PRIMARY KEY,
            created_at TIMESTAMP DEFAULT now(), updated_at TIMESTAMP DEFAULT now(),
            deleted_at TIMESTAMP,
            trip_id BIGINT NOT NULL,
            location VARCHAR(20) NOT NULL,
            received_by VARCHAR(255) NOT NULL,
            received_at TIMESTAMP NOT NULL,
            step_order BIGINT NOT NULL,
            stamped BOOLEAN DEFAULT FALSE,
            notes TEXT
        );
        CREATE TABLE public.receipt_batches (
            id SERIAL PRIMARY KEY,
            driver_id BIGINT NOT NULL,
            status VARCHAR(20) DEFAULT 'pending',
            scanned_at TIMESTAMP DEFAULT now(),
            assigned_to_trip_id BIGINT,
            created_at TIMESTAMP DEFAULT now(), updated_at TIMESTAMP DEFAULT now(),
            deleted_at TIMESTAMP
        );
        CREATE TABLE public.receipts (
            id SERIAL PRIMARY KEY,
            batch_id BIGINT NOT NULL,
            image_path VARCHAR(500) NOT NULL,
            created_at TIMESTAMP DEFAULT now(), updated_at TIMESTAMP DEFAULT now(),
            deleted_at TIMESTAMP
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
/// A FalconGo-shaped admin token at permission 4.
pub fn admin_token(user_id: i64) -> String {
    token_with_permission(user_id, 4)
}

/// The same, at an arbitrary permission level — for testing the gates rather
/// than passing through them.
pub fn token_with_permission(user_id: i64, permission: i32) -> String {
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
            permission,
            exp: (Utc::now() + chrono::Duration::hours(1)).timestamp(),
            iss: user_id.to_string(),
        },
        &EncodingKey::from_secret(JWT_SECRET.as_bytes()),
    )
    .unwrap()
}

/// Trips, routes and fee mappings covering every awkward corner of the four
/// revenue formulas. Shared by the parity suite and the trips-list suite so the
/// two are asserting against identical data.
pub async fn seed_trip_fixture(pool: &PgPool) {
    sqlx::raw_sql(
        r#"
        -- Fee mappings. `fee` means something different per company: a rate in
        -- EGP/1000L for Petrol Arrows, a BAND NUMBER for Watanya, and nothing
        -- at all for TAQA/Petromin (which bill on distance).
        INSERT INTO fee_mappings (company, terminal, drop_off_point, distance, fee) VALUES
            ('Petrol Arrows', 'PA-T1', 'PA-Near',  120.0, 30.5),
            ('Petrol Arrows', 'PA-T1', 'PA-Far',   340.0, 44.25),
            ('Watanya',       'WA-T1', 'WA-B1',    100.0, 1),
            ('Watanya',       'WA-T1', 'WA-B2',    150.0, 2),
            ('Watanya',       'WA-T1', 'WA-B15',   900.0, 15),
            ('TAQA',          'TQ-T1', 'TQ-Site',  210.0, 0),
            ('Petromin',      'PM-T1', 'PM-Site',  180.0, 0);
        -- Deliberately NOT mapped: ('Petrol Arrows','PA-T1','PA-Unmapped') and
        -- ('Watanya','WA-T1','WA-Unmapped'). Both must yield 0.0 revenue via
        -- COALESCE/ELSE, not a NULL that poisons the SUM.

        /* ---- Petrol Arrows: tank_capacity * fee / 1000 ---- */
        INSERT INTO trips (company, terminal, drop_off_point, car_no_plate, tank_capacity, date, receipt_no) VALUES
            ('Petrol Arrows','PA-T1','PA-Near',     'PA-A', 40000, '2025-05-05', 'PA1'),
            ('Petrol Arrows','PA-T1','PA-Near',     'PA-A', 36000, '2025-05-06', 'PA2'),
            ('Petrol Arrows','PA-T1','PA-Far',      'PA-B', 50000, '2025-05-07', 'PA3'),
            ('Petrol Arrows','PA-T1','PA-Unmapped', 'PA-B', 45000, '2025-05-08', 'PA4');

        /* ---- Watanya: tank_capacity * band_rate / 1000 ---- */
        INSERT INTO trips (company, terminal, drop_off_point, car_no_plate, tank_capacity, date, receipt_no) VALUES
            ('Watanya','WA-T1','WA-B1',       'WA-A', 40000, '2025-05-05', 'WA1'),
            ('Watanya','WA-T1','WA-B2',       'WA-A', 40000, '2025-05-06', 'WA2'),
            ('Watanya','WA-T1','WA-B15',      'WA-B', 30000, '2025-05-07', 'WA3'),
            ('Watanya','WA-T1','WA-Unmapped', 'WA-B', 30000, '2025-05-08', 'WA4');

        /* ---- Watanya multi-container: one logical trip, three receipts ----
           Counted ONCE by total_trips but its volume/revenue all count. */
        INSERT INTO trips (company, terminal, drop_off_point, car_no_plate, tank_capacity, date, receipt_no, parent_trip_id) VALUES
            ('Watanya','WA-T1','WA-B1','WA-C', 10000, '2025-05-09', 'WAC1', 9001),
            ('Watanya','WA-T1','WA-B1','WA-C', 10000, '2025-05-09', 'WAC2', 9001),
            ('Watanya','WA-T1','WA-B1','WA-C', 10000, '2025-05-09', 'WAC3', 9001);

        /* ---- TAQA: distance * 50.5, plus a per-car per-MONTH rental ----
           TQ-A works 28 days in May  -> full 43000.
           TQ-B works 10 days in May  -> 43000 - 18 * (43000/28).
           TQ-A also works  3 days in June -> a SECOND, separately tapered
           monthly rental. Summing days across the range instead of per month
           would give one wrong number here, which is the point. */
        INSERT INTO trips (company, terminal, drop_off_point, car_no_plate, tank_capacity, date, receipt_no)
        SELECT 'TAQA','TQ-T1','TQ-Site','TQ-A', 30000,
               to_char(DATE '2025-05-01' + (n || ' days')::interval, 'YYYY-MM-DD'),
               'TQA' || n
        FROM generate_series(0, 27) AS n;

        INSERT INTO trips (company, terminal, drop_off_point, car_no_plate, tank_capacity, date, receipt_no)
        SELECT 'TAQA','TQ-T1','TQ-Site','TQ-B', 25000,
               to_char(DATE '2025-05-01' + (n || ' days')::interval, 'YYYY-MM-DD'),
               'TQB' || n
        FROM generate_series(0, 9) AS n;

        INSERT INTO trips (company, terminal, drop_off_point, car_no_plate, tank_capacity, date, receipt_no)
        SELECT 'TAQA','TQ-T1','TQ-Site','TQ-A', 30000,
               to_char(DATE '2025-06-01' + (n || ' days')::interval, 'YYYY-MM-DD'),
               'TQAJ' || n
        FROM generate_series(0, 2) AS n;

        /* ---- Petromin: distance * 42.5, plus 2000 per CAR-DAY ----
           PM-A runs twice on 05-18 (ONE car-day, not two) and once on 05-19.
           PM-B runs once on 05-18. Three car-days total => 6000. */
        INSERT INTO trips (company, terminal, drop_off_point, car_no_plate, tank_capacity, date, receipt_no) VALUES
            ('Petromin','PM-T1','PM-Site','PM-A', 20000, '2025-05-18', 'PM1'),
            ('Petromin','PM-T1','PM-Site','PM-A', 20000, '2025-05-18', 'PM2'),
            ('Petromin','PM-T1','PM-Site','PM-A', 20000, '2025-05-19', 'PM3'),
            ('Petromin','PM-T1','PM-Site','PM-B', 20000, '2025-05-18', 'PM4');

        /* ---- The container group's parent header ---- */
        INSERT INTO parent_trips (id, car_id, driver_id, car_no_plate, driver_name,
                                  transporter, tank_capacity, company, terminal, date, author)
        VALUES (9001, 1, 1, 'WA-C', 'Container Driver', 'Apex', 30000,
                'Watanya', 'WA-T1', '2025-05-09', 'fixture');

        /* ---- Soft-deleted row: must be invisible to every formula ---- */
        INSERT INTO trips (company, terminal, drop_off_point, car_no_plate, tank_capacity, date, receipt_no, deleted_at) VALUES
            ('Watanya','WA-T1','WA-B15','WA-Z', 99000, '2025-05-10', 'DEL1', now());
        "#,
    )
    .execute(pool)
    .await
    .expect("seed revenue fixture");
}
