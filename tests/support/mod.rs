//! Test harness: real Postgres databases (one per test), the minimal
//! FalconGo-owned public tables, and a process-global mock of the WhatsApp Go
//! API that serves the vendored production corpus.

use actix_web::{web, App, HttpResponse, HttpServer};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::sync::{Arc, Mutex, Once, OnceLock};

pub const TARGET_JID: &str = "201000000000@s.whatsapp.net";
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
            -- The three papers, stored as TEXT 'YYYY-MM-DD' exactly as the Go
            -- backend writes them — including the '' that means "never
            -- recorded", which the dashboard's shape guard has to survive.
            license_expiration_date TEXT DEFAULT '',
            calibration_expiration_date TEXT DEFAULT '',
            tank_license_expiration_date TEXT DEFAULT '',
            created_at TIMESTAMPTZ DEFAULT now(), updated_at TIMESTAMPTZ DEFAULT now(),
            deleted_at TIMESTAMPTZ
        );
        -- `mileage` is the service INTERVAL in km, not distance driven; the
        -- legacy name the Go backend still writes.
        CREATE TABLE public.oil_changes (
            id SERIAL PRIMARY KEY, car_id BIGINT, car_no_plate TEXT,
            date TEXT, mileage NUMERIC, odometer_at_change NUMERIC,
            current_odometer NUMERIC,
            oil_filter_changed BOOLEAN DEFAULT false,
            fuel_filter_changed BOOLEAN DEFAULT false,
            water_filter_changed BOOLEAN DEFAULT false,
            -- The attention sheet reads these off the latest record.
            driver_name TEXT DEFAULT '', super_visor TEXT DEFAULT '',
            cost NUMERIC DEFAULT 0,
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
            petroapp_bill_id BIGINT,
            -- Production orders fuel events by (date, time) and reads the
            -- odometer pair; without these the dashboard's own query fails and
            -- every test in tests/dashboard.rs 500s.
            time TEXT, odometer_before BIGINT, odometer_after BIGINT, fuel_rate NUMERIC
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

    sqlx::raw_sql(FALCON_PRICING_DDL)
        .execute(&pool)
        .await
        .expect("FalconGo stored-pricing schema");

    pool
}

/// FalconGo's stored-pricing schema (stage A, `Models/pricing.go`
/// `MigratePricing`, startup phase 15), copied verbatim: the pricing tables,
/// the stored columns on trips, the seed rules and the `trip_revenue` view.
/// apex-rust reads revenue from these and computes none of it, so the tests
/// need the exact view FalconGo serves — a hand-simplified copy would test a
/// different allocation than production runs.
///
/// Keep in step with FalconGo when the view changes.
pub const FALCON_PRICING_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS pricing_rules (
    id                 bigserial   PRIMARY KEY,
    company            text        NOT NULL,
    effective_from     date        NOT NULL,
    method             text        NOT NULL
        CHECK (method IN ('per_1000l_fee', 'band_per_1000l', 'per_km')),
    per_km_rate        numeric,
    vat_rate           numeric     NOT NULL,
    rental_mode        text        NOT NULL DEFAULT 'none'
        CHECK (rental_mode IN ('none', 'monthly_taper', 'per_car_day')),
    rental_amount      numeric,
    rental_full_days   integer,
    notes              text        NOT NULL DEFAULT '',
    created_by_user_id bigint      NOT NULL DEFAULT 0,
    created_at         timestamptz NOT NULL DEFAULT now(),
    UNIQUE (company, effective_from)
);
CREATE TABLE IF NOT EXISTS pricing_bands (
    id             bigserial PRIMARY KEY,
    rule_id        bigint    NOT NULL REFERENCES pricing_rules(id) ON DELETE CASCADE,
    band           integer   NOT NULL,
    rate_per_1000l numeric   NOT NULL,
    UNIQUE (rule_id, band)
);
CREATE TABLE IF NOT EXISTS truck_month_charges (
    id                bigserial   PRIMARY KEY,
    car_no_plate      text        NOT NULL,
    car_id            bigint,
    company           text        NOT NULL,
    terminal          text        NOT NULL,
    month             date        NOT NULL,
    days_worked       integer     NOT NULL,
    month_days_worked integer     NOT NULL,
    chargeable_days   numeric     NOT NULL,
    rental            numeric     NOT NULL,
    rental_vat        numeric     NOT NULL,
    rule_id           bigint      REFERENCES pricing_rules(id),
    computed_at       timestamptz NOT NULL DEFAULT now(),
    frozen_at         timestamptz,
    UNIQUE (car_no_plate, company, terminal, month)
);
CREATE INDEX IF NOT EXISTS idx_truck_month_charges_month
    ON truck_month_charges (month, company);
CREATE TABLE IF NOT EXISTS pricing_jobs (
    name        text        PRIMARY KEY,
    finished_at timestamptz NOT NULL DEFAULT now(),
    detail      text        NOT NULL DEFAULT ''
);
ALTER TABLE trips ADD COLUMN IF NOT EXISTS base_revenue numeric;
ALTER TABLE trips ADD COLUMN IF NOT EXISTS revenue_vat numeric;
ALTER TABLE trips ADD COLUMN IF NOT EXISTS priced_km numeric;
ALTER TABLE trips ADD COLUMN IF NOT EXISTS pricing_rule_id bigint;
ALTER TABLE trips ADD COLUMN IF NOT EXISTS priced_at timestamptz;
ALTER TABLE trips ADD COLUMN IF NOT EXISTS pricing_error text;
DO $$ BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'fk_trips_pricing_rule_id') THEN
        ALTER TABLE trips ADD CONSTRAINT fk_trips_pricing_rule_id
            FOREIGN KEY (pricing_rule_id) REFERENCES pricing_rules(id);
    END IF;
END $$;
CREATE INDEX IF NOT EXISTS idx_trips_logical_trip
    ON trips ((COALESCE(NULLIF(parent_trip_id, 0), -id)));
CREATE INDEX IF NOT EXISTS idx_trips_company_date
    ON trips (company, date) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_trips_unpriced
    ON trips (date) WHERE deleted_at IS NULL AND pricing_error IS NOT NULL;
INSERT INTO pricing_rules
    (company, effective_from, method, per_km_rate, vat_rate,
     rental_mode, rental_amount, rental_full_days, notes)
 VALUES
    ('Petrol Arrows', '2000-01-01', 'per_1000l_fee', NULL, 0,
     'none', NULL, NULL, 'seed: apex-rust revenue.rs (fee = EGP per 1,000 L, no VAT)'),
    ('Watanya', '2000-01-01', 'band_per_1000l', NULL, 0.14,
     'none', NULL, NULL, 'seed: apex-rust revenue.rs WATANYA_BAND_RATES'),
    ('TAQA', '2000-01-01', 'per_km', 50.5, 0.14,
     'monthly_taper', 43000, 28, 'seed: apex-rust revenue.rs TAQA_*'),
    ('Petromin', '2000-01-01', 'per_km', 42.5, 0.14,
     'per_car_day', 2000, NULL, 'seed: apex-rust revenue.rs PETROMIN_*')
 ON CONFLICT (company, effective_from) DO NOTHING;
INSERT INTO pricing_bands (rule_id, band, rate_per_1000l)
 SELECT r.id, b.band, b.rate
   FROM pricing_rules r
   CROSS JOIN (VALUES
        (1, 104.5), (2, 122.1), (3, 129.8), (4, 156.2), (5, 183.7),
        (6, 196.9), (7, 210.1), (8, 235.4), (9, 261.8), (10, 288.2),
        (11, 314.6), (12, 341.0), (13, 367.4), (14, 393.8), (15, 420.2)) AS b(band, rate)
  WHERE r.company = 'Watanya' AND r.effective_from = '2000-01-01'
 ON CONFLICT (rule_id, band) DO NOTHING;

CREATE OR REPLACE VIEW trip_revenue AS
WITH car_months AS (
    SELECT car_no_plate, company, to_char(month, 'YYYY-MM') AS ym,
           SUM(rental)::numeric     AS rental,
           SUM(rental_vat)::numeric AS rental_vat,
           MAX(month_days_worked)   AS month_days
      FROM truck_month_charges
     GROUP BY car_no_plate, company, to_char(month, 'YYYY-MM')
),
rows AS (
    SELECT t.id, t.company, t.date, t.car_no_plate, t.terminal,
           t.base_revenue, t.revenue_vat, t.priced_km, t.pricing_error,
           COUNT(*) OVER (PARTITION BY t.company, t.car_no_plate, t.date) AS rows_that_day
      FROM trips t
     WHERE t.deleted_at IS NULL
)
SELECT r.id AS trip_id, r.company, r.date, r.car_no_plate, r.terminal,
       r.base_revenue, r.revenue_vat, r.priced_km, r.pricing_error,
       COALESCE(cm.rental     / NULLIF(cm.month_days, 0) / r.rows_that_day, 0) AS allocated_rental,
       COALESCE(r.revenue_vat, 0)
         + COALESCE(cm.rental_vat / NULLIF(cm.month_days, 0) / r.rows_that_day, 0) AS allocated_vat,
       COALESCE(r.base_revenue, 0)
         + COALESCE(cm.rental     / NULLIF(cm.month_days, 0) / r.rows_that_day, 0)
         + COALESCE(r.revenue_vat, 0)
         + COALESCE(cm.rental_vat / NULLIF(cm.month_days, 0) / r.rows_that_day, 0) AS allocated_total
  FROM rows r
  LEFT JOIN car_months cm
    ON cm.car_no_plate = r.car_no_plate
   AND cm.company      = r.company
   AND cm.ym           = left(r.date, 7);
"#;

/// Store a trip's priced figures the way FalconGo's engine writes them:
/// `base_revenue`, `revenue_vat`, `priced_km`, the rule, `priced_at`, and
/// `revenue = COALESCE(base, 0)`. `base = None` is an unpriced trip, which the
/// engine stores with a `pricing_error`. Matches rows by receipt number.
///
/// The figures are literals the caller states, never computed here: these
/// tests check that apex-rust READS what FalconGo stored, so the expected
/// values come from FalconGo's own parity run, not from a formula on this
/// side.
pub async fn store_priced(pool: &PgPool, rows: &[(&str, Option<f64>, f64, Option<f64>)]) {
    for (receipt, base, vat, km) in rows {
        sqlx::query(
            "UPDATE trips t SET
                 base_revenue    = $2::float8::numeric,
                 revenue_vat     = CASE WHEN $2::float8 IS NULL THEN NULL ELSE $3::float8::numeric END,
                 priced_km       = $4::float8::numeric,
                 pricing_error   = CASE WHEN $2::float8 IS NULL
                                        THEN 'no fee mapping for ' || t.company || ' / ' || t.terminal
                                             || ' -> ' || t.drop_off_point END,
                 pricing_rule_id = (SELECT id FROM pricing_rules r WHERE r.company = t.company
                                     ORDER BY effective_from DESC LIMIT 1),
                 priced_at       = now(),
                 revenue         = COALESCE($2::float8, 0)::numeric
             WHERE t.receipt_no = $1 AND t.deleted_at IS NULL",
        )
        .bind(receipt)
        .bind(base)
        .bind(vat)
        .bind(km)
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("store priced {receipt}: {e}"));
    }
}

/// Store one truck-month of car rental, as FalconGo's engine writes it
/// (rental and its VAT rounded to 6 dp).
pub async fn store_truck_month(
    pool: &PgPool,
    car: &str,
    company: &str,
    terminal: &str,
    month: &str,
    days_worked: i32,
    month_days_worked: i32,
    chargeable_days: f64,
    rental: f64,
    rental_vat: f64,
) {
    sqlx::query(
        "INSERT INTO truck_month_charges
             (car_no_plate, company, terminal, month, days_worked, month_days_worked,
              chargeable_days, rental, rental_vat, rule_id, frozen_at)
         VALUES ($1, $2, $3, $4::date, $5, $6, ROUND($7::numeric, 6), ROUND($8::numeric, 6),
                 ROUND($9::numeric, 6),
                 (SELECT id FROM pricing_rules WHERE company = $2 ORDER BY effective_from DESC LIMIT 1),
                 now())",
    )
    .bind(car)
    .bind(company)
    .bind(terminal)
    .bind(month)
    .bind(days_worked)
    .bind(month_days_worked)
    .bind(chargeable_days)
    .bind(rental)
    .bind(rental_vat)
    .execute(pool)
    .await
    .unwrap_or_else(|e| panic!("store truck-month {car} {month}: {e}"));
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

    store_fixture_pricing(pool).await;
}

/// What FalconGo's pricing engine stored for [`seed_trip_fixture`].
///
/// These are the figures FalconGo PR #4 (`pricing/pricing_test.go`,
/// `TestParityWithApexRustGolden`) reports its backfill writing for this exact
/// fixture — copied in as literals, not recomputed: PA-Near 1220 + 1098 =
/// 2318.00, PA-Far 2212.50; Watanya band 1 4180 + 3 x 1045 = 7315.00 with VAT
/// 1024.10; every TAQA row 210 km x 50.5 = 10605.00; every Petromin row 180 km
/// x 42.5 = 7650.00; the two unmapped routes unpriced (NULL, with a
/// pricing_error); and the five truck-months PM-A 4000, PM-B 2000, TQ-A May
/// 43000, TQ-B May 15357.142857, TQ-A June 4607.142857.
///
/// So the golden snapshot passing means apex-rust reads FalconGo's stored
/// values and aggregates them to the same statistics the old formulas
/// produced — not that two copies of a formula agree.
pub async fn store_fixture_pricing(pool: &PgPool) {
    let none = None;
    let mut rows: Vec<(&str, Option<f64>, f64, Option<f64>)> = vec![
        ("PA1", Some(1220.0), 0.0, none),
        ("PA2", Some(1098.0), 0.0, none),
        ("PA3", Some(2212.5), 0.0, none),
        ("PA4", None, 0.0, none),
        ("WA1", Some(4180.0), 585.2, none),
        ("WA2", Some(4884.0), 683.76, none),
        ("WA3", Some(12606.0), 1764.84, none),
        ("WA4", None, 0.0, none),
        ("WAC1", Some(1045.0), 146.3, none),
        ("WAC2", Some(1045.0), 146.3, none),
        ("WAC3", Some(1045.0), 146.3, none),
        ("PM1", Some(7650.0), 1071.0, Some(180.0)),
        ("PM2", Some(7650.0), 1071.0, Some(180.0)),
        ("PM3", Some(7650.0), 1071.0, Some(180.0)),
        ("PM4", Some(7650.0), 1071.0, Some(180.0)),
    ];
    let taqa: Vec<String> = (0..28)
        .map(|n| format!("TQA{n}"))
        .chain((0..10).map(|n| format!("TQB{n}")))
        .chain((0..3).map(|n| format!("TQAJ{n}")))
        .collect();
    for r in &taqa {
        rows.push((r.as_str(), Some(10605.0), 1484.7, Some(210.0)));
    }
    store_priced(pool, &rows).await;

    for (car, company, terminal, month, days, rental, vat) in [
        ("TQ-A", "TAQA", "TQ-T1", "2025-05-01", 28, 43000.0, 6020.0),
        ("TQ-B", "TAQA", "TQ-T1", "2025-05-01", 10, 15357.142857, 2150.0),
        ("TQ-A", "TAQA", "TQ-T1", "2025-06-01", 3, 4607.142857, 645.0),
        ("PM-A", "Petromin", "PM-T1", "2025-05-01", 2, 4000.0, 560.0),
        ("PM-B", "Petromin", "PM-T1", "2025-05-01", 1, 2000.0, 280.0),
    ] {
        store_truck_month(pool, car, company, terminal, month, days, days, days as f64, rental, vat).await;
    }
}
