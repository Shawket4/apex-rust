//! Integration suite against a real local Postgres.
//!
//! The tests the brief demanded, in the order the bugs actually happened:
//! resumption (kill mid-batch), per-template dates (unit tests in the lib),
//! idempotency (everything twice), the real API shape (via the mock server),
//! corpus acceptance with EXTRACTED VALUES asserted against the production
//! oracle, and a full cutover rehearsal.

mod support;

use apex::ingest::poller;
use apex::parser::{self, Verdict};
use rust_decimal::Decimal;
use sqlx::Row;
use std::str::FromStr;

/* ------------------------------------------------------------------------ */
/* Corpus acceptance: ingest the full production corpus through the mock     */
/* API, then assert the split AND every extracted value against the oracle.  */
/* ------------------------------------------------------------------------ */

#[actix_web::test]
async fn corpus_ingests_with_exact_verdicts_and_values() {
    let mock = support::init();
    let _guard = mock.guard.lock().unwrap();
    let pool = support::fresh_db("apex_bsms_corpus").await;
    apex::boot::run_banksms_migrations(&pool).await.unwrap();

    {
        let mut s = mock.state.lock().unwrap();
        s.messages = support::load_corpus();
        s.fail_after_pages = None;
    }

    let client = apex::ingest::WhatsAppClient::from_config();
    let created = poller::poll_once(&pool, &client).await.expect("poll");

    // The frozen corpus: 815 messages = 98 matched + 2 suppressed + 715 ignored.
    let raw_counts: Vec<(String, i64)> =
        sqlx::query_as("SELECT status, COUNT(*) FROM banksms.raw_messages GROUP BY status")
            .fetch_all(&pool)
            .await
            .unwrap();
    let count = |s: &str| {
        raw_counts
            .iter()
            .find(|(k, _)| k == s)
            .map(|(_, n)| *n)
            .unwrap_or(0)
    };
    assert_eq!(count("matched"), 98, "matched split");
    assert_eq!(count("suppressed"), 2, "suppressed split");
    assert_eq!(count("ignored"), 715, "ignored split");
    assert_eq!(created.len(), 98, "one transaction per matched message");

    // Values, not vibes: every whatsapp transaction the old system parsed
    // must reproduce exactly (the oracle is the production dump).
    let mut checked = 0;
    for o in support::load_oracle() {
        let Some(expected_dir) = &o.direction else {
            continue;
        }; // the 2 old partials
        let Some(row) = sqlx::query(
            "SELECT t.direction, t.amount::text AS amount, t.currency, t.account,
                    t.counterparty, t.reference, t.occurred_at, r.status, r.body
             FROM banksms.raw_messages r
             LEFT JOIN banksms.transactions t ON t.raw_message_id = r.id
             WHERE r.wa_message_id = $1",
        )
        .bind(&o.wa_message_id)
        .fetch_optional(&pool)
        .await
        .unwrap() else {
            // The oracle was dumped a few hours after the corpus snapshot;
            // rows that postdate the corpus simply aren't in the fixture.
            continue;
        };

        let status: String = row.get("status");
        let body: String = row.get("body");
        if apex::parser::petroapp_pattern().is_match(&apex::parser::normalize::normalize(&body)) {
            // Noise-reclassified wallet rows: parsed once historically, now
            // recognized-and-suppressed. No transaction expected.
            assert_eq!(status, "suppressed", "petroapp row {}", o.wa_message_id);
            continue;
        }

        assert_eq!(status, "matched", "message {}", o.wa_message_id);
        let got_dir: Option<String> = row.get("direction");
        assert_eq!(
            got_dir.as_deref(),
            Some(expected_dir.as_str()),
            "direction {}",
            o.wa_message_id
        );
        let got_amount: Option<String> = row.get("amount");
        assert_eq!(
            Decimal::from_str(&got_amount.unwrap()).unwrap(),
            Decimal::from_str(o.amount.as_ref().unwrap()).unwrap(),
            "amount {}",
            o.wa_message_id
        );
        assert_eq!(
            row.get::<Option<String>, _>("currency").as_deref(),
            o.currency.as_deref(),
            "currency {}",
            o.wa_message_id
        );
        assert_eq!(
            row.get::<Option<String>, _>("account"),
            o.account,
            "account {}",
            o.wa_message_id
        );
        assert_eq!(
            row.get::<Option<String>, _>("counterparty"),
            o.counterparty,
            "counterparty {}",
            o.wa_message_id
        );
        assert_eq!(
            row.get::<Option<String>, _>("reference"),
            o.reference,
            "reference {}",
            o.wa_message_id
        );
        assert_eq!(
            row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("occurred_at"),
            o.occurred_at,
            "occurred_at {}",
            o.wa_message_id
        );
        checked += 1;
    }
    assert!(
        checked >= 90,
        "only {checked} oracle rows verified — oracle not exercised"
    );

    // The two formerly-stuck messages now parse completely.
    for (needle, dir, amount) in [
        ("شراء لحظي", "out", "135.72"),
        ("إلى حسابك المنتهي", "in", "5700.00"),
    ] {
        let row = sqlx::query(
            "SELECT t.direction, t.amount::text AS amount
             FROM banksms.raw_messages r
             JOIN banksms.transactions t ON t.raw_message_id = r.id
             WHERE r.body LIKE '%' || $1 || '%'",
        )
        .bind(needle)
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|_| panic!("gap message '{needle}' has no transaction"));
        assert_eq!(row.get::<String, _>("direction"), dir);
        assert_eq!(
            Decimal::from_str(&row.get::<String, _>("amount")).unwrap(),
            Decimal::from_str(amount).unwrap()
        );
    }

    // Idempotency: the same poll again inserts nothing and creates nothing.
    let created_again = poller::poll_once(&pool, &client).await.expect("re-poll");
    assert_eq!(created_again.len(), 0, "second poll must be a no-op");
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM banksms.raw_messages")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(total, 815, "no duplicates on replay");

    // Reparse sweep is a no-op when nothing changed.
    let (changed, created) = poller::reparse_sweep(&pool).await.unwrap();
    assert_eq!((changed, created), (0, 0), "sweep must be a no-op");
}

/* ------------------------------------------------------------------------ */
/* Resumption: crash mid-cycle must replay, never skip.                      */
/* ------------------------------------------------------------------------ */

#[actix_web::test]
async fn killed_mid_batch_replays_without_skips_or_duplicates() {
    let mock = support::init();
    let _guard = mock.guard.lock().unwrap();
    let pool = support::fresh_db("apex_bsms_resume").await;
    apex::boot::run_banksms_migrations(&pool).await.unwrap();

    {
        let mut s = mock.state.lock().unwrap();
        s.messages = support::load_corpus();
        s.fail_after_pages = Some(2); // die on the third page
    }

    let client = apex::ingest::WhatsAppClient::from_config();
    let err = poller::poll_once(&pool, &client).await;
    assert!(err.is_err(), "the simulated crash must surface");

    // Two pages landed; the cursor must NOT have advanced.
    let partial: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM banksms.raw_messages")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(partial, 200, "two pages committed before the crash");
    let cursor_ts: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT last_wa_timestamp FROM banksms.ingest_cursor WHERE id = 1")
            .fetch_optional(&pool)
            .await
            .unwrap()
            .flatten();
    assert!(
        cursor_ts.is_none(),
        "cursor advanced despite the crash — the silent-skip bug"
    );

    // Recovery: the next full cycle picks up everything, exactly once.
    mock.state.lock().unwrap().fail_after_pages = None;
    poller::poll_once(&pool, &client)
        .await
        .expect("recovery poll");
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM banksms.raw_messages")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(total, 815, "zero skips, zero duplicates after recovery");
    let matched: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM banksms.raw_messages WHERE status = 'matched'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(matched, 98);
}

/* ------------------------------------------------------------------------ */
/* Cutover rehearsal: legacy schema -> v2, in one verify-or-rollback tx.     */
/* ------------------------------------------------------------------------ */

#[actix_web::test]
async fn cutover_rehearsal_folds_overrides_and_survives_migrator() {
    support::init();
    let pool = support::fresh_db("apex_bsms_cutover").await;
    support::apply_legacy_schema(&pool).await;

    // Synthetic legacy state, shaped like production's edge cases.
    sqlx::raw_sql(
        r#"
        -- A parsed arabic_ipn with a full set of human overrides (txn 10).
        INSERT INTO banksms.raw_messages (id, wa_message_id, chat_jid, wa_timestamp, body, parse_status)
        VALUES (1, 'MSG_PARSED', '201280701070@s.whatsapp.net', '2026-08-09T10:18:21Z',
                'يرجى العلم أنه تم خصم مبلغ EGP 9000.00 من حساب ********9276 عبر شبكة المدفوعات اللحظية (IPN) من خلال خدمات الإنترنت البنكية بتاريخ 09-08-2026 13:18 إلى عماد ج... ي... ج... برقم مرجعي FT26221BLW0C للمزيد، يرجي الاتصال بـ 19666.',
                'parsed');
        INSERT INTO banksms.transactions
            (id, source, raw_message_id, version, parsed_direction, parsed_amount, parsed_currency,
             parsed_account, parsed_counterparty, parsed_reference, parsed_occurred_at,
             parsed_template, parse_method, created_at, updated_at)
        VALUES (10, 'whatsapp', 1, 3, 'out', 9000.00, 'EGP', '9276', 'عماد ج... ي... ج...',
                'FT26221BLW0C', '2026-08-09T10:18:00Z', 'arabic_ipn', 'template',
                '2026-08-09T10:19:00Z', '2026-08-10T07:54:00Z');
        INSERT INTO banksms.transaction_overrides (transaction_id, field, value, actor, set_at)
        VALUES (10, 'occurred_at', '2026-08-09T09:00:00+00:00', '3', '2026-08-10T07:54:34Z'),
               (10, 'amount', '9000', '3', '2026-08-10T07:54:34Z');

        -- The stuck partial: amount known, direction unknown (txn 20).
        INSERT INTO banksms.raw_messages (id, wa_message_id, chat_jid, wa_timestamp, body, parse_status)
        VALUES (2, 'MSG_PARTIAL', '201280701070@s.whatsapp.net', '2026-08-10T09:16:02Z',
                'يرجى العلم انه تم تنفيذ عملية شراء لحظي من حسابك المنتهي بـ ********5447 بمبلغ 135.72 جم من Mobile Recharge برقم مرجعي cdec344b بتاريخ 10-08-2026 12:15 الرصيد المتاح 85635.98 جم للمزيد، برجاء الاتصال بـ 19666. ',
                'partial');
        INSERT INTO banksms.transactions
            (id, source, raw_message_id, version, parsed_amount, parse_method, created_at, updated_at)
        VALUES (20, 'whatsapp', 2, 1, 135.72, 'extractors', now(), now());

        -- A PetroApp wallet transfer, ignored in the legacy schema (no txn).
        INSERT INTO banksms.raw_messages (id, wa_message_id, chat_jid, wa_timestamp, body, parse_status)
        VALUES (3, 'MSG_PETROAPP', '201280701070@s.whatsapp.net', '2026-08-10T07:50:50Z',
                'تم تنفيذ تحويل لحظي من حسابكم رقم 0180 بمبلغ 70000.00 جم إلى petroapp c** رقم مرجعي 552921316865 يوم 08-10 الساعة 10:50 للمزيد اتصل بـ 19623',
                'ignored');

        -- Chatter (stays ignored) and an import row.
        INSERT INTO banksms.raw_messages (id, wa_message_id, chat_jid, wa_timestamp, body, parse_status)
        VALUES (4, 'MSG_CHATTER', '201280701070@s.whatsapp.net', '2026-08-11T09:02:42Z',
                'هو لازم contact', 'ignored');
        INSERT INTO banksms.transactions
            (id, source, import_source_id, version, parsed_direction, parsed_amount,
             parsed_currency, parsed_occurred_at, category, parse_method, created_at, updated_at)
        VALUES (30, 'import', 77, 1, 'out', 500.00, 'EGP', '2025-12-01T22:00:00Z', 'Labor',
                'manual', now(), now());

        -- A soft-deleted whatsapp transaction (txn 40) on a parsed raw.
        INSERT INTO banksms.raw_messages (id, wa_message_id, chat_jid, wa_timestamp, body, parse_status)
        VALUES (5, 'MSG_DELETED', '201280701070@s.whatsapp.net', '2026-08-08T11:32:20Z',
                'Transfer reference #4b19beef of EGP 2000.00 has been debited from your account 6001-01 through IPN on 09/08/2026 at 08:16, your available balance is 953.48. For inquiries, call 19555.',
                'parsed');
        INSERT INTO banksms.transactions
            (id, source, raw_message_id, version, parsed_direction, parsed_amount, parsed_currency,
             parsed_account, parsed_reference, parsed_occurred_at, parsed_template, parse_method,
             deleted_at, created_at, updated_at)
        VALUES (40, 'whatsapp', 5, 1, 'out', 2000.00, 'EGP', '6001-01', '4b19beef',
                '2026-08-09T05:16:00Z', 'ref_balance', 'template', now(), now(), now());

        INSERT INTO banksms.ingest_cursor (id, chat_jid, last_wa_timestamp, last_wa_message_id)
        VALUES (1, '201280701070@s.whatsapp.net', '2026-08-11T09:02:42Z', 'MSG_CHATTER')
        ON CONFLICT (id) DO UPDATE SET
            chat_jid = EXCLUDED.chat_jid,
            last_wa_timestamp = EXCLUDED.last_wa_timestamp,
            last_wa_message_id = EXCLUDED.last_wa_message_id;
        "#,
    )
    .execute(&pool)
    .await
    .expect("legacy fixture data");

    apex::cutover::run(&pool).await.expect("cutover");

    // Renamed, and the new schema answered.
    let legacy_there: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('banksms_legacy.raw_messages')::text")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        legacy_there.is_some(),
        "legacy schema retained for the soak"
    );

    // Override fold: occurred_at from the override, provenance stamped.
    let r = sqlx::query(
        "SELECT occurred_at, amount::text AS amount, edited_by, version
         FROM banksms.transactions WHERE id = 10",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        r.get::<chrono::DateTime<chrono::Utc>, _>("occurred_at")
            .to_rfc3339(),
        "2026-08-09T09:00:00+00:00"
    );
    assert_eq!(
        r.get::<Option<String>, _>("edited_by").as_deref(),
        Some("3")
    );
    assert_eq!(r.get::<i32, _>("version"), 3, "version copied verbatim");

    // The partial re-parsed to completion by the new template.
    let r = sqlx::query(
        "SELECT direction, amount::text AS amount, currency, counterparty
         FROM banksms.transactions WHERE id = 20",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(r.get::<String, _>("direction"), "out");
    assert_eq!(
        Decimal::from_str(&r.get::<String, _>("amount")).unwrap(),
        Decimal::from_str("135.72").unwrap()
    );
    assert_eq!(r.get::<String, _>("currency"), "EGP");
    assert_eq!(
        r.get::<Option<String>, _>("counterparty").as_deref(),
        Some("Mobile Recharge")
    );

    // Statuses recomputed: petroapp suppressed, chatter ignored, parsed matched.
    let status = |id: i64| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>("SELECT status FROM banksms.raw_messages WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(status(1).await, "matched");
    assert_eq!(status(2).await, "matched");
    assert_eq!(status(3).await, "suppressed");
    assert_eq!(status(4).await, "ignored");

    // Soft delete preserved.
    let deleted: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT deleted_at FROM banksms.transactions WHERE id = 40")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(deleted.is_some());

    // Sequences restarted past the copied ids.
    let probe: i64 = sqlx::query_scalar(
        "INSERT INTO banksms.transactions (source, direction, amount, occurred_at)
         VALUES ('manual', 'out', 1, now()) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        probe > 40,
        "sequence must clear the copied ids, got {probe}"
    );

    // The stamped bookkeeping satisfies the real migrator (checksum included).
    apex::boot::run_banksms_migrations(&pool)
        .await
        .expect("migrator must treat the stamped baseline as applied");

    // Running the cutover twice is refused at preflight.
    let second = apex::cutover::run(&pool).await;
    assert!(second.is_err(), "second cutover must refuse");
}

/* ------------------------------------------------------------------------ */
/* API flows: registration lifecycle, If-Match, promotion.                   */
/* ------------------------------------------------------------------------ */

#[actix_web::test]
async fn api_registration_promotion_and_concurrency() {
    use actix_web::{test, web, App};

    support::init();
    let pool = support::fresh_db("apex_bsms_api").await;
    apex::boot::run_banksms_migrations(&pool).await.unwrap();

    sqlx::raw_sql(
        "INSERT INTO public.employees (id, name) VALUES (12, 'عماد جرجس');
         INSERT INTO public.drivers (id, name) VALUES (7, 'سامى نصحى');
         INSERT INTO banksms.raw_messages (id, wa_message_id, chat_jid, wa_timestamp, body, status)
         VALUES (900, 'MSG_PROMOTE', '201280701070@s.whatsapp.net', '2026-08-11T09:02:42Z',
                 'تعديل لخصم قطس كاوتش', 'ignored');",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .configure(apex::api::configure),
    )
    .await;
    let token = support::admin_token(3);
    let auth = ("Authorization", format!("Bearer {token}"));

    // Create an Advance without a person → 400 with a human reason.
    let req = test::TestRequest::post()
        .uri("/api/v1/transactions")
        .insert_header(auth.clone())
        .set_json(serde_json::json!({
            "direction": "out", "amount": "9000", "occurred_at": "2026-08-09T09:00:00Z",
            "category": "Advance"
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400);

    // With an employee → 201 and a loans row of kind advance.
    let req = test::TestRequest::post()
        .uri("/api/v1/transactions")
        .insert_header(auth.clone())
        .set_json(serde_json::json!({
            "direction": "out", "amount": "9000", "occurred_at": "2026-08-09T09:00:00Z",
            "category": "Advance", "employee_id": 12, "description": "سلفة عماد"
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = test::read_body_json(resp).await;
    let txn_id = body["id"].as_i64().unwrap();
    let loan_id = body["loan"]["id"].as_i64().expect("loan registered");
    assert_eq!(body["loan"]["kind"], "advance");

    let loan = sqlx::query(
        "SELECT amount, kind, employee_id, is_paid, date FROM public.loans WHERE id = $1",
    )
    .bind(loan_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(loan.get::<f64, _>("amount"), 9000.0);
    assert_eq!(loan.get::<String, _>("kind"), "advance");
    assert_eq!(loan.get::<Option<i32>, _>("employee_id"), Some(12));
    // Cairo calendar day of 2026-08-09T09:00Z (+03:00 in August) is the 9th.
    assert_eq!(loan.get::<String, _>("date"), "2026-08-09");

    // PATCH without If-Match → 428; with a stale one → 409.
    let req = test::TestRequest::patch()
        .uri(&format!("/api/v1/transactions/{txn_id}"))
        .insert_header(auth.clone())
        .set_json(serde_json::json!({ "amount": "9500" }))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 428);
    let req = test::TestRequest::patch()
        .uri(&format!("/api/v1/transactions/{txn_id}"))
        .insert_header(auth.clone())
        .insert_header(("If-Match", "42"))
        .set_json(serde_json::json!({ "amount": "9500" }))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 409);

    // A proper edit syncs the unpaid loan.
    let req = test::TestRequest::patch()
        .uri(&format!("/api/v1/transactions/{txn_id}"))
        .insert_header(auth.clone())
        .insert_header(("If-Match", "1"))
        .set_json(serde_json::json!({ "amount": "9500" }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let synced: f64 = sqlx::query_scalar("SELECT amount FROM public.loans WHERE id = $1")
        .bind(loan_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(synced, 9500.0, "unpaid loan follows the edit");

    // Once settled, money edits are refused with 409.
    sqlx::query("UPDATE public.loans SET is_paid = true WHERE id = $1")
        .bind(loan_id)
        .execute(&pool)
        .await
        .unwrap();
    let req = test::TestRequest::patch()
        .uri(&format!("/api/v1/transactions/{txn_id}"))
        .insert_header(auth.clone())
        .insert_header(("If-Match", "2"))
        .set_json(serde_json::json!({ "amount": "1" }))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 409);
    // ... and so is deleting the transaction.
    let req = test::TestRequest::delete()
        .uri(&format!("/api/v1/transactions/{txn_id}"))
        .insert_header(auth.clone())
        .insert_header(("If-Match", "2"))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 409);

    // Unsettle → delete cascades to the loan, softly.
    sqlx::query("UPDATE public.loans SET is_paid = false WHERE id = $1")
        .bind(loan_id)
        .execute(&pool)
        .await
        .unwrap();
    let req = test::TestRequest::delete()
        .uri(&format!("/api/v1/transactions/{txn_id}"))
        .insert_header(auth.clone())
        .insert_header(("If-Match", "2"))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 204);
    let gone: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT deleted_at FROM public.loans WHERE id = $1")
            .bind(loan_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(gone.is_some(), "loan soft-deleted with its transaction");

    // Promotion: record an ignored message; a second recording is refused.
    let req = test::TestRequest::post()
        .uri("/api/v1/transactions")
        .insert_header(auth.clone())
        .set_json(serde_json::json!({
            "raw_message_id": 900, "direction": "out", "amount": "250.00",
            "occurred_at": "2026-08-11T09:02:42Z", "description": "قطس كاوتش"
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);
    let req = test::TestRequest::post()
        .uri("/api/v1/transactions")
        .insert_header(auth.clone())
        .set_json(serde_json::json!({
            "raw_message_id": 900, "direction": "out", "amount": "99",
            "occurred_at": "2026-08-11T09:02:42Z"
        }))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        409,
        "one transaction per message, ever"
    );

    // The messages screen shows it as recorded.
    let req = test::TestRequest::get()
        .uri("/api/v1/messages/900")
        .insert_header(auth.clone())
        .to_request();
    let msg: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    assert!(msg["transaction_id"].as_i64().is_some());
    assert_eq!(
        msg["status"], "ignored",
        "parser verdict untouched by the human decision"
    );

    // No token → rejected.
    let req = test::TestRequest::get()
        .uri("/api/v1/transactions")
        .to_request();
    let resp = test::try_call_service(&app, req).await;
    match resp {
        Ok(r) => assert_eq!(r.status(), 401),
        Err(e) => assert_eq!(e.as_response_error().status_code(), 401),
    }
}

/* ------------------------------------------------------------------------ */
/* Direct parser check on the fixtures the seeds carry (fast sanity).        */
/* ------------------------------------------------------------------------ */

#[actix_web::test]
async fn every_seeded_template_passes_its_own_sample() {
    support::init();
    let pool = support::fresh_db("apex_bsms_seeds").await;
    apex::boot::run_banksms_migrations(&pool).await.unwrap();

    let broken = apex::parser::templates::boot_check(&pool).await.unwrap();
    assert!(
        broken.is_empty(),
        "templates failed their samples: {broken:?}"
    );

    // And the wallet sample suppresses (recognized, then excluded).
    let templates = apex::parser::templates::load(&pool).await.unwrap();
    let wallet = templates
        .iter()
        .find(|t| t.name == "wallet_transfer")
        .unwrap();
    let verdict = parser::parse(&wallet.sample.clone(), &templates, Some(chrono::Utc::now()));
    match verdict {
        Verdict::Suppressed { template } => assert_eq!(template, "wallet_transfer"),
        other => panic!("wallet sample must suppress, got {:?}", other.status()),
    }
}
