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
            ADD COLUMN slack_status TEXT DEFAULT '', ADD COLUMN geo_fence TEXT DEFAULT '',
            ADD COLUMN tank_capacity INTEGER DEFAULT 0;
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
         INSERT INTO cars (id, car_no_plate, operating_company, operating_area, driver_id, latitude, longitude, etit_car_id, tank_capacity)
         VALUES (1, 'ق ط ر 1234', 'Watanya', 'Suez', 1, '29.97', '32.53', 'etit-1', 38000),
                (2, 'ع ب ج 5678', 'Mobil', 'Cairo', NULL, '', '', '', 0);
         INSERT INTO terminals (id, name, latitude, longitude) VALUES (10, 'Suez Terminal', 29.96, 32.55);
         -- Watanya's fee is a band number; its rates are in the pricing rule.
         INSERT INTO fee_mappings (company, terminal, terminal_id, drop_off_point, distance, fee)
         VALUES ('Watanya', 'Suez Terminal', 10, 'Station A', 50, 1),
                ('Watanya', 'Suez Terminal', 10, 'Station B', 200, 2),
                ('Watanya', 'Suez Terminal', 10, 'Station Z', 90, 40),
                ('Mobil', 'Mostorod', NULL, 'Station M', 30, 900);
         -- Revenue as FalconGo stored it (base before VAT, and its 14% VAT).
         INSERT INTO trips (car_id, car_no_plate, driver_name, company, terminal, drop_off_point, date,
                            revenue, base_revenue, revenue_vat, mileage)
         VALUES (1, 'ق ط ر 1234', 'أحمد', 'Watanya', 'Suez Terminal', 'Station A', to_char(current_date - 2, 'YYYY-MM-DD'), 1500, 1500, 210, 100),
                (1, 'ق ط ر 1234', 'أحمد', 'Watanya', 'Suez Terminal', 'Station B', to_char(current_date - 1, 'YYYY-MM-DD'), 3000, 3000, 420, 400);
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
    advisor_with(falcon, "http://127.0.0.1:9", "http://127.0.0.1:9")
}

fn advisor_with(falcon: &str, maint: &str, apex: &str) -> Advisor {
    Advisor {
        provider: None,
        http: reqwest::Client::new(),
        cfg: AdvisorConfig {
            falcon_api_url: falcon.to_string(),
            etit_proxy_url: "http://127.0.0.1:9".into(),
            etit_admin_token: None,
            maint_api_url: maint.to_string(),
            apex_api_url: apex.to_string(),
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
    let mut adv = offline_advisor("http://127.0.0.1:9");
    // The truck's position comes from its tracker.
    adv.cfg.etit_proxy_url = fake_etit(Arc::new(Mutex::new(Vec::new()))).await;
    let token = apex::advisor::upstream::BearerToken("t".into());
    let c = ctx(&pool, &adv, &token);

    let found = tools::run(&c, "find_truck", &json!({ "text": "1234" })).await;
    assert_eq!(found[0]["car_id"], 1, "{found}");
    assert_eq!(found[0]["driver_name"], "أحمد");

    let overview = tools::run(&c, "fleet_overview", &json!({ "days": 30 })).await;
    let watanya = overview["companies"].as_array().unwrap().iter().find(|r| r["operating_company"] == "Watanya").unwrap();
    assert_eq!(watanya["trips"], 2, "{overview}");
    // Base, VAT and total are reported apart, from the stored figures.
    assert_eq!(watanya["base_revenue"], 4500);
    assert_eq!(watanya["vat"], 630);
    assert_eq!(watanya["rental"], 0);
    assert_eq!(watanya["total_revenue"], 5130);

    let from = (chrono::Utc::now().date_naive() - chrono::Duration::days(10)).to_string();
    let to = chrono::Utc::now().date_naive().to_string();
    let rev = tools::run(&c, "revenue_breakdown", &json!({ "from": from, "to": to, "group_by": "car" })).await;
    let row = &rev["rows"][0];
    assert_eq!(row["base_revenue"], 4500, "{rev}");
    assert_eq!(row["total_revenue"], 5130);
    assert_eq!(row["fuel_cost"], 800);
    // On the total, rental and VAT included — and the tool says so.
    assert_eq!(row["margin_before_driver"], 4330);
    assert!(rev["margin_basis"].as_str().unwrap().contains("total_revenue"));

    let options = tools::run(&c, "route_options", &json!({ "car_id": 1 })).await;
    let first = &options["options"][0];
    // Priced from Watanya's rule for this 38,000 L tank: band 1 is
    // 104.5 x 38 = 3971 over 2x50 km + empty, which beats band 2's
    // 122.1 x 38 = 4639.8 over 2x200 km + empty. Never fee / km.
    assert_eq!(first["drop_off_point"], "Station A", "{options}");
    assert_eq!(first["band"], 1);
    assert_eq!(first["expected_base_revenue"], 3971.0);
    assert_eq!(first["expected_vat"], 555.94);
    assert_eq!(first["empty_km_method"], "straight_line_x1.3");
    assert_eq!(options["truck_position_known"], true);
    assert_eq!(options["tank_capacity_l"], 38000);
    let second = &options["options"][1];
    assert_eq!(second["expected_base_revenue"], 4639.8);
    // A band the rule has no rate for is listed as unpriced, not ranked at 0.
    let z = &options["unpriced_routes"][0];
    assert_eq!(z["drop_off_point"], "Station Z", "{options}");
    assert!(z["why"].as_str().unwrap().contains("band 40"));
    // A company with no pricing rule cannot be priced at all.
    let mobil = tools::run(&c, "route_options", &json!({ "car_id": 2 })).await;
    assert!(mobil["error"].as_str().unwrap().contains("no pricing rule"), "{mobil}");

    let bad = tools::run(&c, "revenue_breakdown", &json!({ "from": "yesterday", "to": to, "group_by": "car" })).await;
    assert!(bad["error"].as_str().unwrap().contains("YYYY-MM-DD"));
}

/// What a company is charged, version by version.
#[tokio::test]
async fn pricing_rules_answer_what_we_charge() {
    let pool = db("advisor_pricing_rules").await;
    // A future Watanya version: band 5 goes up from 1 December.
    sqlx::raw_sql(
        "INSERT INTO pricing_rules (company, effective_from, method, vat_rate, notes)
         VALUES ('Watanya', (current_date + 40), 'band_per_1000l', 0.14, 'band 5 up');
         INSERT INTO pricing_bands (rule_id, band, rate_per_1000l)
         SELECT id, 5, 190 FROM pricing_rules WHERE notes = 'band 5 up';",
    )
    .execute(&pool)
    .await
    .unwrap();
    let adv = offline_advisor("http://127.0.0.1:9");
    let token = apex::advisor::upstream::BearerToken("t".into());
    let c = ctx(&pool, &adv, &token);

    let out = tools::run(&c, "pricing_rules", &json!({ "company": "Watanya" })).await;
    let wa = &out["companies"][0];
    assert_eq!(wa["company"], "Watanya", "{out}");
    let now = &wa["in_force"];
    assert_eq!(now["method"], "band_per_1000l");
    assert_eq!(now["vat_rate"], 0.14);
    let band5 = now["bands"].as_array().unwrap().iter().find(|b| b["band"] == 5).unwrap();
    assert_eq!(band5["rate_per_1000l"], 183.7);
    assert_eq!(wa["future"][0]["notes"], "band 5 up");

    // On a date after it starts, the new version is the one in force.
    let later = (chrono::Utc::now().date_naive() + chrono::Duration::days(60)).to_string();
    let out = tools::run(&c, "pricing_rules", &json!({ "company": "Watanya", "on_date": later })).await;
    assert_eq!(out["companies"][0]["in_force"]["notes"], "band 5 up");
    assert_eq!(out["companies"][0]["history"].as_array().unwrap().len(), 1);

    let all = tools::run(&c, "pricing_rules", &json!({})).await;
    assert_eq!(all["companies"].as_array().unwrap().len(), 4, "{all}");
    let taqa = all["companies"].as_array().unwrap().iter().find(|c| c["company"] == "TAQA").unwrap();
    assert_eq!(taqa["in_force"]["rental_mode"], "monthly_taper");
    assert_eq!(taqa["in_force"]["rental_amount"], 43000.0);

    let none = tools::run(&c, "pricing_rules", &json!({ "company": "Mobil" })).await;
    assert!(none["error"].is_string());
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

    // Permission 3 cannot approve: the route's floor refuses it before the
    // handler runs (a middleware error, which a server sends as 403).
    let manager = support::token_with_permission(42, 3);
    let req = test::TestRequest::post()
        .uri(&format!("/api/v1/advisor/suggestions/{id}/approve"))
        .insert_header(("Authorization", format!("Bearer {manager}")))
        .to_request();
    let refused = match test::try_call_service(&app, req).await {
        Ok(resp) => resp.status(),
        Err(e) => e.as_response_error().status_code(),
    };
    assert_eq!(refused, 403);
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

/// A stand-in maint-rust: one oil change at sync_version 3, its audit log, and
/// a sync endpoint that answers each batch with `outcome` per operation.
async fn fake_maint(seen: Arc<Mutex<Vec<Value>>>, row_id: String, outcome: &'static str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = HttpServer::new(move || {
        let seen = seen.clone();
        let row_id = row_id.clone();
        App::new().default_service(web::to(move |req: HttpRequest, body: web::Bytes| {
            let seen = seen.clone();
            let row_id = row_id.clone();
            async move {
                let path = req.path().to_string();
                if path == "/api/maint/ai-audit" {
                    return HttpResponse::Ok().json(json!([]));
                }
                if path == format!("/api/maint/records/oil_changes/{row_id}") {
                    return HttpResponse::Ok().json(json!({
                        "entity_type": "oil_changes", "entity_id": row_id, "sync_version": 3,
                        "row": { "id": row_id, "vehicle_id": 1, "liters": 30, "oil_type": "15W40" }
                    }));
                }
                if path.starts_with("/api/maint/records/") {
                    return HttpResponse::NotFound().finish();
                }
                let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let h = |k: &str| req.headers().get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                seen.lock().unwrap().push(json!({ "path": path, "via": h("x-via-ai"), "body": b }));
                let results: Vec<Value> = b["operations"].as_array().map(|ops| ops.iter().map(|o| json!({
                    "status": outcome, "entity_id": o["entity_id"], "new_sync_version": 4
                })).collect()).unwrap_or_default();
                HttpResponse::Ok().json(json!({ "results": results }))
            }
        }))
    })
    .listen(listener)
    .unwrap()
    .run();
    tokio::spawn(server);
    format!("http://127.0.0.1:{port}/api/maint")
}

async fn approve(pool: &PgPool, adv: &Advisor, id: uuid::Uuid) -> Value {
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
    test::call_and_read_body_json(&app, req).await
}

#[actix_web::test]
async fn a_maint_edit_carries_the_row_version_and_fails_on_a_conflict() {
    let pool = db("advisor_maint").await;
    let row_id = uuid::Uuid::new_v4().to_string();
    let token = apex::advisor::upstream::BearerToken("t".into());

    for (outcome, want) in [("applied", "applied"), ("conflict", "failed")] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let maint = fake_maint(seen.clone(), row_id.clone(), outcome).await;
        let adv = advisor_with("http://127.0.0.1:9", &maint, "http://127.0.0.1:9");
        let c = ctx(&pool, &adv, &token);
        let proposed = tools::run(&c, "propose_change", &json!({
            "kind": "maint_record",
            "params": { "entity_type": "oil_changes", "operation": "update", "entity_id": row_id, "fields": { "liters": 35 } },
            "title": "Fix liters", "rationale": "The receipt says 35 L"
        })).await;
        let id: uuid::Uuid = serde_json::from_value(proposed["suggestion_id"].clone()).unwrap_or_else(|_| panic!("{proposed}"));
        let s = store::suggestion(&pool, id).await.unwrap().unwrap();
        assert_eq!(s.target, "maint");
        assert_eq!(s.before["liters"], 30, "{}", s.before);
        assert_eq!(s.after["liters"], 35);

        let row = approve(&pool, &adv, id).await;
        assert_eq!(row["status"], want, "{row}");
        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["path"], "/api/maint/sync/push");
        assert_eq!(calls[0]["via"], "advisor");
        assert_eq!(calls[0]["body"]["operations"][0]["sync_version"], 3);
        if want == "failed" {
            assert!(row["result"]["error"].as_str().unwrap().contains("changed"), "{row}");
        }
    }

    // A record that is not there is refused at proposal time.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let maint = fake_maint(seen, row_id.clone(), "applied").await;
    let adv = advisor_with("http://127.0.0.1:9", &maint, "http://127.0.0.1:9");
    let c = ctx(&pool, &adv, &token);
    let missing = tools::run(&c, "propose_change", &json!({
        "kind": "maint_record",
        "params": { "entity_type": "oil_changes", "operation": "delete", "entity_id": uuid::Uuid::new_v4().to_string() },
        "title": "x", "rationale": "y"
    })).await;
    assert!(missing["error"].as_str().unwrap().contains("no maint-rust"), "{missing}");
}

#[actix_web::test]
async fn a_ledger_edit_goes_through_apex_routes_with_its_version_and_is_audited() {
    let pool = db("advisor_ledger").await;

    // apex-rust's own bank-SMS routes, served for real on a loopback port.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let served = pool.clone();
    let server = HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(served.clone()))
            .configure(apex::api::configure)
    })
    .listen(listener)
    .unwrap()
    .run();
    tokio::spawn(server);
    let apex_url = format!("http://127.0.0.1:{port}");

    let http = reqwest::Client::new();
    let admin = support::admin_token(42);
    // The permission floor: a manager (3) can no longer write the ledger.
    let manager = support::token_with_permission(43, 3);
    let body = json!({ "direction": "out", "amount": "500", "occurred_at": "2026-10-20T09:00:00Z", "description": "toll" });
    let r = http.post(format!("{apex_url}/api/v1/transactions")).bearer_auth(&manager).json(&body).send().await.unwrap();
    assert_eq!(r.status(), 403);
    let r = http.post(format!("{apex_url}/api/v1/transactions")).bearer_auth(&admin).json(&body).send().await.unwrap();
    assert_eq!(r.status(), 201);
    let txn: Value = r.json().await.unwrap();
    let txn_id = txn["id"].as_i64().unwrap();

    let adv = advisor_with("http://127.0.0.1:9", "http://127.0.0.1:9", &apex_url);
    let token = apex::advisor::upstream::BearerToken(admin.clone());
    let c = ctx(&pool, &adv, &token);
    let propose = |desc: &'static str| {
        json!({ "kind": "edit_transaction", "params": { "id": txn_id, "description": desc },
                "title": "Describe the toll", "rationale": "SMS says Suez road" })
    };
    let proposed = tools::run(&c, "propose_change", &propose("Suez road toll")).await;
    let id: uuid::Uuid = serde_json::from_value(proposed["suggestion_id"].clone()).unwrap_or_else(|_| panic!("{proposed}"));
    let s = store::suggestion(&pool, id).await.unwrap().unwrap();
    assert_eq!(s.target, "apex");
    assert_eq!(s.before["description"], "toll");
    assert!(s.apply_meta["if_match"].is_number(), "{}", s.apply_meta);

    let row = approve(&pool, &adv, id).await;
    assert_eq!(row["status"], "applied", "{row}");
    let (desc, edited_by): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT description, edited_by FROM banksms.transactions WHERE id = $1")
            .bind(txn_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(desc.as_deref(), Some("Suez road toll"));
    assert_eq!(edited_by.as_deref(), Some("42 via AI advisor"));
    let (user, status, via): (Option<i64>, Option<i32>, String) = sqlx::query_as(
        "SELECT user_id, status, via FROM advisor.ai_audit_log WHERE suggestion_id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((user, status, via.as_str()), (Some(42), Some(200), "advisor"));

    // Proposed, then someone edits the row by hand: the stale version is
    // refused and nothing is overwritten.
    let stale = tools::run(&c, "propose_change", &propose("AI wording")).await;
    let stale_id: uuid::Uuid = serde_json::from_value(stale["suggestion_id"].clone()).unwrap();
    let current: Value = http.get(format!("{apex_url}/api/v1/transactions/{txn_id}")).bearer_auth(&admin).send().await.unwrap().json().await.unwrap();
    let r = http
        .patch(format!("{apex_url}/api/v1/transactions/{txn_id}"))
        .bearer_auth(&admin)
        .header("If-Match", current["version"].to_string())
        .json(&json!({ "description": "human wording" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let row = approve(&pool, &adv, stale_id).await;
    assert_eq!(row["status"], "failed", "{row}");
    assert_eq!(row["result_status"], 409);
    let desc: Option<String> = sqlx::query_scalar("SELECT description FROM banksms.transactions WHERE id = $1")
        .bind(txn_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(desc.as_deref(), Some("human wording"));
}

/// A stand-in etit-proxy: one live truck parked at the Suez terminal and one
/// inside a dead zone; the zone scan records the body it was sent.
async fn fake_etit(scans: Arc<Mutex<Vec<Value>>>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = HttpServer::new(move || {
        let scans = scans.clone();
        App::new().default_service(web::to(move |req: HttpRequest, body: web::Bytes| {
            let scans = scans.clone();
            async move {
                let now = chrono::Utc::now();
                match req.path() {
                    "/api/v1/vehicles/live" => HttpResponse::Ok().json(json!([
                        { "id": "etit-1", "lat": 29.9602, "lng": 32.5503, "speed": 0, "status": 13,
                          "timestamp": now.to_rfc3339(), "event": "Ignition On" },
                        { "id": "etit-2", "lat": 30.5, "lng": 31.5, "speed": 0, "status": 3,
                          "timestamp": (now - chrono::Duration::hours(3)).to_rfc3339() }
                    ])),
                    "/api/v1/zones" => HttpResponse::Ok().json(json!([
                        { "id": 7, "name": "Quarry road", "lat": 30.5, "lng": 31.5, "radius_m": 800, "active": true }
                    ])),
                    "/api/v1/zones/scan" => {
                        scans.lock().unwrap().push(serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null));
                        HttpResponse::Ok().json(json!({
                            "days_scanned": 3, "vehicles_scanned": 2,
                            "violations": [{ "vehicle_id": "etit-1", "codename": "X/1234", "zone_name": "Quarry road",
                                             "entry_local": "2026-10-20 14:02:00", "exit_local": "2026-10-20 14:20:00", "dwell_secs": 1080 }]
                        }))
                    }
                    _ => HttpResponse::NotFound().finish(),
                }
            }
        }))
    })
    .listen(listener)
    .unwrap()
    .run();
    tokio::spawn(server);
    format!("http://127.0.0.1:{port}")
}

#[actix_web::test]
async fn fleet_status_reads_the_trackers_not_the_stale_columns() {
    let pool = db("advisor_fleet").await;
    sqlx::query("UPDATE cars SET etit_car_id = 'etit-2' WHERE id = 2").execute(&pool).await.unwrap();
    let scans = Arc::new(Mutex::new(Vec::new()));
    let etit = fake_etit(scans.clone()).await;
    let mut adv = offline_advisor("http://127.0.0.1:9");
    adv.cfg.etit_proxy_url = etit;
    let token = apex::advisor::upstream::BearerToken("t".into());
    let c = ctx(&pool, &adv, &token);

    let status = tools::run(&c, "fleet_status", &json!({})).await;
    let trucks = status["trucks"].as_array().unwrap_or_else(|| panic!("{status}"));
    let one = trucks.iter().find(|t| t["car_id"] == 1).unwrap();
    assert_eq!(one["status"], "idling", "{one}");
    assert_eq!(one["at_place"]["name"], "Suez Terminal");
    let two = trucks.iter().find(|t| t["car_id"] == 2).unwrap();
    // Three hours since its last fix: no signal, whatever the status code says.
    assert_eq!(two["status"], "no_signal", "{two}");
    assert_eq!(two["in_dead_zone"]["name"], "Quarry road");
    assert_eq!(status["trucks_in_dead_zones"], 1);

    let zones = tools::run(&c, "dead_zones", &json!({ "from": "2026-10-18", "to": "2026-10-20" })).await;
    assert_eq!(zones["inside_now"][0]["car_id"], 2, "{zones}");
    assert_eq!(zones["violations"][0]["plate"], "ق ط ر 1234");
    assert_eq!(zones["violations"][0]["dwell_min"], 18.0);
    // Always a range: an empty body would be the alerting round.
    assert_eq!(scans.lock().unwrap()[0], json!({ "from": "2026-10-18", "to": "2026-10-20" }));

    // The stale columns are refused with a pointer to the live tool.
    let stale = tools::run(&c, "run_sql", &json!({ "query": "SELECT car_no_plate, slack_status FROM cars" })).await;
    assert!(stale["error"].as_str().unwrap().contains("fleet_status"), "{stale}");
}
