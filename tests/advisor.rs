//! The advisor against a real Postgres: its schema, the read-only SQL guard,
//! the fleet tools, the suggestion lifecycle, keyword search, and the approve
//! path end to end — a fake FalconGo checks that the change arrives with the
//! approver's token and the AI headers.

mod support;

use std::sync::{Arc, Mutex};

use actix_web::{test, web, App, HttpRequest, HttpResponse, HttpServer};
use serde_json::{json, Value};
use sqlx::PgPool;

use apex::advisor::{search, sql, store, tools, Advisor, AdvisorConfig};

async fn db(name: &str) -> PgPool {
    support::init();
    let pool = support::fresh_db(name).await;
    apex::boot::run_banksms_migrations(&pool)
        .await
        .expect("migrations (banksms + advisor)");
    sqlx::raw_sql(
        "ALTER TABLE cars
            ADD COLUMN car_type TEXT DEFAULT '', ADD COLUMN operating_company TEXT DEFAULT '',
            ADD COLUMN operating_area TEXT DEFAULT '', ADD COLUMN is_in_trip BOOLEAN DEFAULT false,
            ADD COLUMN driver_id INTEGER, ADD COLUMN latitude TEXT DEFAULT '',
            ADD COLUMN longitude TEXT DEFAULT '', ADD COLUMN location_time_stamp TEXT DEFAULT '',
            ADD COLUMN slack_status TEXT DEFAULT '', ADD COLUMN geo_fence TEXT DEFAULT '';
         ALTER TABLE fee_mappings ADD COLUMN terminal_id INTEGER;
         CREATE TABLE terminals (
            id SERIAL PRIMARY KEY, name TEXT, address TEXT,
            latitude DOUBLE PRECISION, longitude DOUBLE PRECISION, radius_m DOUBLE PRECISION,
            created_at TIMESTAMPTZ DEFAULT now(), updated_at TIMESTAMPTZ DEFAULT now(),
            deleted_at TIMESTAMPTZ);
         CREATE TABLE services (
            id SERIAL PRIMARY KEY, car_id BIGINT, service_type TEXT, date_of_service TEXT,
            cost NUMERIC, created_at TIMESTAMPTZ DEFAULT now(), updated_at TIMESTAMPTZ DEFAULT now(),
            deleted_at TIMESTAMPTZ);
         ALTER TABLE fuel_events ADD COLUMN car_id BIGINT;
         CREATE TABLE service_invoices (
            id SERIAL PRIMARY KEY, car_id INTEGER, date TIMESTAMPTZ, meter_reading BIGINT,
            plate_number TEXT, created_at TIMESTAMPTZ DEFAULT now(),
            updated_at TIMESTAMPTZ DEFAULT now(), deleted_at TIMESTAMPTZ);
         CREATE TABLE inspection_items (
            id SERIAL PRIMARY KEY, service_invoice_id INTEGER, service TEXT, notes TEXT,
            created_at TIMESTAMPTZ DEFAULT now(), updated_at TIMESTAMPTZ DEFAULT now(),
            deleted_at TIMESTAMPTZ);

         INSERT INTO drivers (id, name) VALUES (1, 'أحمد'), (2, 'محمود');
         INSERT INTO cars (id, car_no_plate, operating_company, operating_area, driver_id, latitude, longitude, etit_car_id)
         VALUES (1, 'ق ط ر 1234', 'Watanya', 'Suez', 1, '29.97', '32.53', 'etit-1'),
                (2, 'ع ب ج 5678', 'Mobil', 'Cairo', NULL, '', '', '');
         INSERT INTO terminals (id, name, latitude, longitude) VALUES (10, 'Suez Terminal', 29.96, 32.55);
         INSERT INTO fee_mappings (company, terminal, terminal_id, drop_off_point, distance, fee)
         VALUES ('Watanya', 'Suez Terminal', 10, 'Station A', 50, 1500),
                ('Watanya', 'Suez Terminal', 10, 'Station B', 200, 3000),
                ('Mobil', 'Mostorod', NULL, 'Station M', 30, 900);
         INSERT INTO trips (car_id, car_no_plate, driver_name, company, terminal, drop_off_point, date, revenue, mileage)
         VALUES (1, 'ق ط ر 1234', 'أحمد', 'Watanya', 'Suez Terminal', 'Station A', to_char(current_date - 2, 'YYYY-MM-DD'), 1500, 100),
                (1, 'ق ط ر 1234', 'أحمد', 'Watanya', 'Suez Terminal', 'Station B', to_char(current_date - 1, 'YYYY-MM-DD'), 3000, 400);
         INSERT INTO fuel_events (car_id, date, price) VALUES (1, to_char(current_date - 1, 'YYYY-MM-DD'), 800);
         INSERT INTO service_invoices (id, car_id, date, plate_number) VALUES (1, 1, now(), 'ق ط ر 1234');
         INSERT INTO inspection_items (service_invoice_id, service, notes)
         VALUES (1, 'تغيير تيل فرامل امامي', NULL), (1, 'لحام شاسيه', 'خلفي'), (1, 'تغيير هوبة', NULL);",
    )
    .execute(&pool)
    .await
    .expect("advisor seed");
    pool
}

/// An advisor with no model and neighbours at a closed port.
fn offline_advisor(falcon: &str) -> Advisor {
    Advisor {
        provider: None,
        http: reqwest::Client::new(),
        cfg: AdvisorConfig {
            falcon_api_url: falcon.to_string(),
            etit_proxy_url: "http://127.0.0.1:9".into(),
            etit_admin_token: None,
            maint_api_url: "http://127.0.0.1:9".into(),
            tei_url: "http://127.0.0.1:9/embed".into(),
            digest_enabled: false,
        },
    }
}

fn ctx<'a>(pool: &'a PgPool, adv: &'a Advisor, token: &'a apex::advisor::upstream::BearerToken) -> tools::ToolCtx<'a> {
    tools::ToolCtx {
        pool,
        adv,
        token,
        user_id: 7,
        conversation_id: None,
        proposed: Mutex::new(Vec::new()),
        events: None,
    }
}

#[tokio::test]
async fn sql_reads_and_cannot_write() {
    let pool = db("advisor_sql").await;
    let ok = sql::run(&pool, "SELECT car_no_plate FROM cars ORDER BY id").await;
    assert_eq!(ok["row_count"], 2);
    let refused = sql::run(&pool, "UPDATE cars SET operating_company = 'x'").await;
    assert!(refused["error"].is_string());
    // A write smuggled past the shape check still dies in the READ ONLY tx.
    let smuggled = sql::run(&pool, "SELECT nextval('cars_id_seq')").await;
    assert!(smuggled["error"].as_str().unwrap_or("").contains("read-only"), "{smuggled}");
}

#[tokio::test]
async fn fleet_tools_answer_from_the_data() {
    let pool = db("advisor_tools").await;
    let adv = offline_advisor("http://127.0.0.1:9");
    let token = apex::advisor::upstream::BearerToken("t".into());
    let c = ctx(&pool, &adv, &token);

    let found = tools::run(&c, "find_truck", &json!({ "text": "1234" })).await;
    assert_eq!(found[0]["car_id"], 1, "{found}");
    assert_eq!(found[0]["driver_name"], "أحمد");

    let overview = tools::run(&c, "fleet_overview", &json!({ "days": 30 })).await;
    let watanya = overview["companies"].as_array().unwrap().iter().find(|r| r["operating_company"] == "Watanya").unwrap();
    assert_eq!(watanya["trips"], 2, "{overview}");
    assert_eq!(watanya["revenue"], 4500);

    let from = (chrono::Utc::now().date_naive() - chrono::Duration::days(10)).to_string();
    let to = chrono::Utc::now().date_naive().to_string();
    let rev = tools::run(&c, "revenue_breakdown", &json!({ "from": from, "to": to, "group_by": "car" })).await;
    let row = &rev["rows"][0];
    assert_eq!(row["revenue"], 4500, "{rev}");
    assert_eq!(row["fuel_cost"], 800);
    assert_eq!(row["margin_before_driver"], 3700);

    let options = tools::run(&c, "route_options", &json!({ "car_id": 1 })).await;
    let first = &options["options"][0];
    // 1500 / (2×50 + empty) beats 3000 / (2×200 + empty) per km.
    assert_eq!(first["drop_off_point"], "Station A", "{options}");
    assert_eq!(first["empty_km_method"], "straight_line_x1.3");
    assert_eq!(options["truck_position_known"], true);

    let bad = tools::run(&c, "revenue_breakdown", &json!({ "from": "yesterday", "to": to, "group_by": "car" })).await;
    assert!(bad["error"].as_str().unwrap().contains("YYYY-MM-DD"));
}

#[tokio::test]
async fn keyword_search_folds_arabic_and_fuses_variants() {
    let pool = db("advisor_search").await;
    let adv = offline_advisor("http://127.0.0.1:9");
    // The embedder is down, so this exercises the keyword lists alone.
    let hits = search::hybrid(
        &pool,
        &adv.http,
        &adv.cfg,
        &["فرامل".into(), "تيل".into(), "هوبه".into()],
        &search::Filters::default(),
        10,
    )
    .await
    .unwrap();
    let services: Vec<&str> = hits.iter().map(|h| h.service.as_str()).collect();
    assert!(services.contains(&"تغيير تيل فرامل امامي"), "{services:?}");
    // هوبه (typed with ه) finds هوبة (written with ة).
    assert!(services.contains(&"تغيير هوبة"), "{services:?}");
    assert!(!services.contains(&"لحام شاسيه"));
    // Found by two variants, the brake-pad line ranks first.
    assert_eq!(hits[0].service, "تغيير تيل فرامل امامي");
}

/// A stand-in FalconGo that records what reached it.
async fn fake_falcon(seen: Arc<Mutex<Vec<Value>>>) -> String {
    fake_falcon_with(seen, true).await
}

/// `audit_live: false` is a FalconGo from before the AI audit log (#2).
async fn fake_falcon_with(seen: Arc<Mutex<Vec<Value>>>, audit_live: bool) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = HttpServer::new(move || {
        let seen = seen.clone();
        App::new().default_service(web::to(move |req: HttpRequest, body: web::Bytes| {
            let seen = seen.clone();
            async move {
                if req.path() == "/api/ai-audit" {
                    return if audit_live {
                        HttpResponse::Ok().json(json!({ "logs": [], "count": 0 }))
                    } else {
                        HttpResponse::NotFound().finish()
                    };
                }
                let h = |k: &str| req.headers().get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                seen.lock().unwrap().push(json!({
                    "method": req.method().as_str(),
                    "path": req.path(),
                    "authorization": h("authorization"),
                    "via": h("x-via-ai"),
                    "suggestion": h("x-ai-suggestion-id"),
                    "body": serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null),
                }));
                HttpResponse::Ok().json(json!({ "id": 1, "operating_company": "Mobil" }))
            }
        }))
    })
    .listen(listener)
    .unwrap()
    .run();
    tokio::spawn(server);
    format!("http://127.0.0.1:{port}/api")
}

#[actix_web::test]
async fn an_approved_suggestion_is_applied_through_falcon_in_the_approvers_name() {
    let pool = db("advisor_apply").await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let falcon = fake_falcon(seen.clone()).await;
    let adv = offline_advisor(&falcon);
    let token = apex::advisor::upstream::BearerToken("t".into());
    let c = ctx(&pool, &adv, &token);

    // The model proposes; a misspelled company is refused before it is stored.
    let typo = tools::run(&c, "propose_change", &json!({
        "kind": "reassign_car", "params": { "car_id": 2, "operating_company": "Watnya" },
        "title": "x", "rationale": "y"
    })).await;
    assert!(typo["error"].as_str().unwrap().contains("Watnya"), "{typo}");
    let proposed = tools::run(&c, "propose_change", &json!({
        "kind": "reassign_car", "params": { "car_id": 2, "operating_company": "Watanya" },
        "title": "Move 5678 to Watanya", "rationale": "Idle 30 days at Mobil; Watanya pays 30 EGP/km"
    })).await;
    let id: uuid::Uuid = serde_json::from_value(proposed["suggestion_id"].clone()).unwrap();
    let s = store::suggestion(&pool, id).await.unwrap().unwrap();
    assert_eq!(s.status, "pending");
    assert_eq!(s.before["operating_company"], "Mobil");
    assert_eq!(s.after["operating_company"], "Watanya");

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(adv.clone()))
            .configure(apex::advisor::handlers::configure),
    )
    .await;

    // Permission 3 cannot approve.
    let manager = support::token_with_permission(42, 3);
    let req = test::TestRequest::post()
        .uri(&format!("/api/v1/advisor/suggestions/{id}/approve"))
        .insert_header(("Authorization", format!("Bearer {manager}")))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 403);
    assert!(seen.lock().unwrap().is_empty());

    // An admin approves: the change goes to FalconGo with their token.
    let admin = support::admin_token(42);
    let req = test::TestRequest::post()
        .uri(&format!("/api/v1/advisor/suggestions/{id}/approve"))
        .insert_header(("Authorization", format!("Bearer {admin}")))
        .to_request();
    let row: Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(row["status"], "applied", "{row}");
    assert_eq!(row["decided_by_user_id"], 42);
    let calls = seen.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["method"], "PATCH");
    assert_eq!(calls[0]["path"], "/api/cars/2/assignment");
    assert_eq!(calls[0]["authorization"], format!("Bearer {admin}"));
    assert_eq!(calls[0]["via"], "advisor");
    assert_eq!(calls[0]["suggestion"], id.to_string());
    assert_eq!(calls[0]["body"], json!({ "operating_company": "Watanya" }));

    // A second approve is a conflict, not a second write.
    let req = test::TestRequest::post()
        .uri(&format!("/api/v1/advisor/suggestions/{id}/approve"))
        .insert_header(("Authorization", format!("Bearer {admin}")))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 409);
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[actix_web::test]
async fn chat_without_a_key_says_so_and_search_falls_back_to_the_words() {
    let pool = db("advisor_http").await;
    let adv = offline_advisor("http://127.0.0.1:9");
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(adv))
            .configure(apex::advisor::handlers::configure),
    )
    .await;
    let admin = support::admin_token(42);
    let req = test::TestRequest::post()
        .uri("/api/v1/advisor/chat")
        .insert_header(("Authorization", format!("Bearer {admin}")))
        .set_json(json!({ "message": "hi" }))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 400);

    // Search is open to any admin user (permission 1), like FalconGo's.
    let viewer = support::token_with_permission(43, 1);
    let req = test::TestRequest::post()
        .uri("/api/v1/advisor/search")
        .insert_header(("Authorization", format!("Bearer {viewer}")))
        .set_json(json!({ "query": "لحام" }))
        .to_request();
    let v: Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(v["variants"], json!(["لحام"]));
    assert_eq!(v["hits"][0]["service"], "لحام شاسيه", "{v}");
}

#[actix_web::test]
async fn without_falcon_audit_nothing_is_applied() {
    let pool = db("advisor_noaudit").await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let falcon = fake_falcon_with(seen.clone(), false).await;
    let adv = offline_advisor(&falcon);
    let token = apex::advisor::upstream::BearerToken("t".into());
    let c = ctx(&pool, &adv, &token);
    let proposed = tools::run(&c, "propose_change", &json!({
        "kind": "assign_driver", "params": { "car_id": 2, "driver_id": 2 },
        "title": "Pair 5678 with محمود", "rationale": "Truck has no driver"
    })).await;
    let id: uuid::Uuid = serde_json::from_value(proposed["suggestion_id"].clone()).unwrap();
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(adv.clone()))
            .configure(apex::advisor::handlers::configure),
    )
    .await;
    let admin = support::admin_token(42);
    let req = test::TestRequest::post()
        .uri(&format!("/api/v1/advisor/suggestions/{id}/approve"))
        .insert_header(("Authorization", format!("Bearer {admin}")))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 409);
    assert!(seen.lock().unwrap().is_empty(), "no write reached FalconGo");
    assert_eq!(store::suggestion(&pool, id).await.unwrap().unwrap().status, "pending");
}
