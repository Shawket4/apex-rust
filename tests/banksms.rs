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
        VALUES (1, 'MSG_PARSED', '201000000000@s.whatsapp.net', '2026-08-09T10:18:21Z',
                'يرجى العلم أنه تم خصم مبلغ EGP 9000.00 من حساب ********2222 عبر شبكة المدفوعات اللحظية (IPN) من خلال خدمات الإنترنت البنكية بتاريخ 09-08-2026 13:18 إلى رمزي ط... ك... ذ... برقم مرجعي FT24377NCY0Q للمزيد، يرجي الاتصال بـ 19666.',
                'parsed');
        INSERT INTO banksms.transactions
            (id, source, raw_message_id, version, parsed_direction, parsed_amount, parsed_currency,
             parsed_account, parsed_counterparty, parsed_reference, parsed_occurred_at,
             parsed_template, parse_method, created_at, updated_at)
        VALUES (10, 'whatsapp', 1, 3, 'out', 9000.00, 'EGP', '2222', 'رمزي ط... ك... ذ...',
                'FT24377NCY0Q', '2026-08-09T10:18:00Z', 'arabic_ipn', 'template',
                '2026-08-09T10:19:00Z', '2026-08-10T07:54:00Z');
        INSERT INTO banksms.transaction_overrides (transaction_id, field, value, actor, set_at)
        VALUES (10, 'occurred_at', '2026-08-09T09:00:00+00:00', '3', '2026-08-10T07:54:34Z'),
               (10, 'amount', '9000', '3', '2026-08-10T07:54:34Z');

        -- The stuck partial: amount known, direction unknown (txn 20).
        INSERT INTO banksms.raw_messages (id, wa_message_id, chat_jid, wa_timestamp, body, parse_status)
        VALUES (2, 'MSG_PARTIAL', '201000000000@s.whatsapp.net', '2026-08-10T09:16:02Z',
                'يرجى العلم انه تم تنفيذ عملية شراء لحظي من حسابك المنتهي بـ ********1111 بمبلغ 135.72 جم من Mekawy Abdallah برقم مرجعي ebfc259e بتاريخ 10-08-2026 12:15 الرصيد المتاح 98013.76 جم للمزيد، برجاء الاتصال بـ 19666. ',
                'partial');
        INSERT INTO banksms.transactions
            (id, source, raw_message_id, version, parsed_amount, parse_method, created_at, updated_at)
        VALUES (20, 'whatsapp', 2, 1, 135.72, 'extractors', now(), now());

        -- A PetroApp wallet transfer, ignored in the legacy schema (no txn).
        INSERT INTO banksms.raw_messages (id, wa_message_id, chat_jid, wa_timestamp, body, parse_status)
        VALUES (3, 'MSG_PETROAPP', '201000000000@s.whatsapp.net', '2026-08-10T07:50:50Z',
                'تم تنفيذ تحويل لحظي من حسابكم رقم 5555 بمبلغ 70000.00 جم إلى petroapp r** رقم مرجعي 203538269989 يوم 08-10 الساعة 10:50 للمزيد اتصل بـ 19623',
                'ignored');

        -- Chatter (stays ignored) and an import row.
        INSERT INTO banksms.raw_messages (id, wa_message_id, chat_jid, wa_timestamp, body, parse_status)
        VALUES (4, 'MSG_CHATTER', '201000000000@s.whatsapp.net', '2026-08-11T09:02:42Z',
                'هو لازم contact', 'ignored');
        INSERT INTO banksms.transactions
            (id, source, import_source_id, version, parsed_direction, parsed_amount,
             parsed_currency, parsed_occurred_at, category, parse_method, created_at, updated_at)
        VALUES (30, 'import', 77, 1, 'out', 500.00, 'EGP', '2025-12-01T22:00:00Z', 'Labor',
                'manual', now(), now());

        -- A soft-deleted whatsapp transaction (txn 40) on a parsed raw.
        INSERT INTO banksms.raw_messages (id, wa_message_id, chat_jid, wa_timestamp, body, parse_status)
        VALUES (5, 'MSG_DELETED', '201000000000@s.whatsapp.net', '2026-08-08T11:32:20Z',
                'Transfer reference #2c92aeed of EGP 2000.00 has been debited from your account 9001-01 through IPN on 09/08/2026 at 08:16, your available balance is 501.37. For inquiries, call 19555.',
                'parsed');
        INSERT INTO banksms.transactions
            (id, source, raw_message_id, version, parsed_direction, parsed_amount, parsed_currency,
             parsed_account, parsed_reference, parsed_occurred_at, parsed_template, parse_method,
             deleted_at, created_at, updated_at)
        VALUES (40, 'whatsapp', 5, 1, 'out', 2000.00, 'EGP', '9001-01', '2c92aeed',
                '2026-08-09T05:16:00Z', 'ref_balance', 'template', now(), now(), now());

        INSERT INTO banksms.ingest_cursor (id, chat_jid, last_wa_timestamp, last_wa_message_id)
        VALUES (1, '201000000000@s.whatsapp.net', '2026-08-11T09:02:42Z', 'MSG_CHATTER')
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
        Some("Mekawy Abdallah")
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
         VALUES (900, 'MSG_PROMOTE', '201000000000@s.whatsapp.net', '2026-08-11T09:02:42Z',
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

/* ------------------------------------------------------------------------ */
/* v3 templates: every format that arrived after the corpus froze, through  */
/* the seeded rows. Messages are invented; the wording is each bank's.      */
/* ------------------------------------------------------------------------ */

#[actix_web::test]
async fn v3_templates_extract_every_new_bank_format() {
    use chrono::{TimeZone, Utc};
    use chrono_tz::Africa::Cairo;

    support::init();
    let pool = support::fresh_db("apex_bsms_v3").await;
    apex::boot::run_banksms_migrations(&pool).await.unwrap();
    let templates = apex::parser::templates::load(&pool).await.unwrap();

    // Arrival in Cairo time; DST (+3) through October 2026.
    let at = |s: &str| {
        Cairo
            .from_local_datetime(
                &chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap(),
            )
            .unwrap()
            .with_timezone(&Utc)
    };

    // (message, arrived, template, direction, amount, account, counterparty, reference, occurred Cairo)
    let cases: &[(&str, &str, &str, &str, &str, Option<&str>, Option<&str>, Option<&str>, &str)] = &[
        ("تم تحويل مبلغ 31500EGP     من حساب رقم xxx6666       فى 18-AUG-2026  عن طريق التحويل اللحظي",
         "2026-08-18 12:41", "bm_transfer_out", "out", "31500", Some("6666"), None, None, "2026-08-18 12:41"),
        // Arrived the next morning: the date stands, the time is unknown.
        ("تم اضافة مبلغ 64000EGP     الى حساب رقم xxx6666      فى 18-AUG-2026  عن طريق التحويل اللحظي",
         "2026-08-19 08:00", "bm_transfer_in", "in", "64000", Some("6666"), None, None, "2026-08-18 00:00"),
        ("تم إضافة تحويل لحظي لحسابكم رقم 5555 بمبلغ 8250.00 جم من KARIM NABIL FAHMY رقم مرجعي 418273645091 يوم 09-03 الساعة 10:17 للمزيد اتصل بـ 19623",
         "2026-09-03 10:17", "instant_credit", "in", "8250.00", Some("5555"), Some("KARIM NABIL FAHMY"), Some("418273645091"), "2026-09-03 10:17"),
        ("تم إضافة تحويل لحظي لحسابكم رقم xxx6666 بمبلغ 64000 جم من KARIM NABIL FAHMY رقم مرجعي 449500000036 يوم 2026-10-05 الساعه 11:42 للمزيد اتصل علي 19888",
         "2026-10-05 11:42", "instant_credit", "in", "64000", Some("6666"), Some("KARIM NABIL FAHMY"), Some("449500000036"), "2026-10-05 11:42"),
        ("تم رد تحويل لحظي لحسابكم رقم 5555 بمبلغ 5.00 جم رقم مرجعي 186200000056 يوم 10-12 الساعة 15:08 للمزيد اتصل بـ 19623",
         "2026-10-12 15:10", "instant_reversal", "in", "5.00", Some("5555"), None, Some("186200000056"), "2026-10-12 15:08"),
        ("تم تنفيذ تحويل لحظي من حسابكم رقم xxx6666 بمبلغ 2500 جم إلى Hany Fawzy رقم مرجعي 551203948776 يوم 2026-10-02 الساعه 10:15 للمزيد اتصل ب 19888 ",
         "2026-10-02 10:15", "wallet_transfer_v2", "out", "2500", Some("6666"), Some("Hany Fawzy"), Some("551203948776"), "2026-10-02 10:15"),
        ("تم خصم مبلغ 1875.30 جم لحظيا باستخدام (شبكة المدفوعات اللحظية) من حسابكم رقم 5555 عند Corner Market يوم 08-22 الساعة16:05 مرجع التاجر 1400000033 للمزيد اتصل بـ 19623.",
         "2026-08-22 16:05", "ipn_merchant", "out", "1875.30", Some("5555"), Some("Corner Market"), Some("1400000033"), "2026-08-22 16:05"),
        ("تم خصم 2500 EGP  من بطاقة الخصم المباشر رقم7777  عندATM 0042 MAIN ST    يوم29/08/26  الساعه11:40  المتاح1204.15EGP  للمزيد إتصل ب ١٩٦٢٣",
         "2026-08-29 11:41", "nbe_card", "out", "2500", Some("7777"), Some("ATM 0042 MAIN ST"), None, "2026-08-29 11:40"),
        ("تم خصم مبلغ  EGP 610.00 من بطاقة الخصم المباشر المنتهية بـ **4444 عند NILE FUEL STATION في 17/08/26 09:25 ، الرصيد المتاح 3150.40 EGP",
         "2026-08-17 09:25", "card_merchant", "out", "610.00", Some("4444"), Some("NILE FUEL STATION"), None, "2026-08-17 09:25"),
        ("تم خصم EGP 355.25  من بطاقة الخصم المباشر # **4444 باستخدام Apple Pay عند  CITY INTERNET في  24/08/26 18:12الرصيد المتاح  EGP2210.05.",
         "2026-08-24 18:12", "card_merchant", "out", "355.25", Some("4444"), Some("CITY INTERNET"), None, "2026-08-24 18:12"),
        ("Your debit card **8888 was debited by EGP 12,400.00 at DELTA PHARMACY 1001 Giza EG. Available balance is EGP 3,105.20.",
         "2026-09-26 13:20", "debit_card_en", "out", "12400.00", Some("8888"), Some("DELTA PHARMACY 1001 Giza"), None, "2026-09-26 13:20"),
        // 12-hour clock: "08:15" arriving 20:17 is 20:15.
        ("Your Credit Card#**3434 was charged EGP 89.00 at STREAMING SVC on 23/09/26 at 08:15. Your available limit is EGP 18800. For more info, call 19322.",
         "2026-09-23 20:17", "credit_card_charge", "out", "89.00", Some("3434"), Some("STREAMING SVC"), None, "2026-09-23 20:15"),
        ("Your ABK-Egypt Credit Card *3434 was credited with EGP 2750.00 on 10/14/26 10:30 with IPN transfer from OMAR TAREK SALEM (ID 6800018).For queries call 19322",
         "2026-10-14 10:30", "credit_card_payment", "in", "2750.00", Some("3434"), Some("OMAR TAREK SALEM"), Some("6800018"), "2026-10-14 10:30"),
        ("Reversal of debit of 412.5 EGP in your ABK-Egypt account *3333 on 10/6/26 16:05 for InstaPay transfer to وليد حازم عزت (ID 4b000001).For queries call 19322",
         "2026-10-06 16:06", "abk_reversal", "in", "412.5", Some("3333"), Some("وليد حازم عزت"), Some("4b000001"), "2026-10-06 16:05"),
        ("You have received a transfer reference #5a000096 with EGP 6500 from TAREK ADEL MANSOUR through IPN on 26/08/2026 at 17:48. For inquiries, call 19555.",
         "2026-08-26 17:48", "ipn_received_en", "in", "6500", None, Some("TAREK ADEL MANSOUR"), Some("5a000096"), "2026-08-26 17:48"),
        ("Purchase reference #0f000001 of EGP 92.40 has been debited from your account 9001-01 through IPN on 30/08/2026 at 12:10, your available balance is 615.33. For inquiries, call 19555.",
         "2026-08-30 12:10", "ref_balance_v2", "out", "92.40", Some("9001-01"), None, Some("0f000001"), "2026-08-30 12:10"),
        // The separately charged IPN fee: one decimal, empty reference.
        ("Transfer reference # of EGP 0.5 has been debited from your account 9001-01 through IPN on 25/09/2026 at 09:05, your available balance is 4210.70. For inquiries, call 19555.",
         "2026-09-25 09:05", "ref_balance_v2", "out", "0.5", Some("9001-01"), None, None, "2026-09-25 09:05"),
        ("عميلنا العزيز، يرجى العلم أن عملية التحويل التي تمت عبر خدمات الإنترنت البنكية من حساب ********2222 بـ EGP85000.00 فى 03-10-2026 11:15 إلى Ayman K........ T... برقم مرجعي FT26274ABCDE لم يتم تنفيذها لحظيًا من خلال شبكة المدفوعات اللحظية (IPN)  وقد تم رد المبلغ إلى حسابكم.",
         "2026-10-03 11:41", "ipn_refund", "in", "85000.00", Some("2222"), Some("Ayman K........ T..."), Some("FT26274ABCDE"), "2026-10-03 11:15"),
        ("Dear Customer, You have initiated an Internal Account Transfer transaction from account XXXXXXXXXXXX1234567 to payee Hany Fawzy with the amount of EGP42,500",
         "2026-08-27 14:02", "internal_transfer_en", "out", "42500", Some("4567"), Some("Hany Fawzy"), None, "2026-08-27 14:02"),
    ];

    let mut seen = std::collections::BTreeSet::new();
    for &(body, arrived, tpl, dir, amount, account, counterparty, reference, occurred) in cases {
        match parser::parse(body, &templates, Some(at(arrived))) {
            Verdict::Matched { template, fields } => {
                assert_eq!(template, tpl, "template for {body}");
                assert_eq!(fields.direction, dir, "direction for {tpl}");
                assert_eq!(
                    fields.amount,
                    Decimal::from_str(amount).unwrap(),
                    "amount for {tpl}"
                );
                assert_eq!(fields.currency, "EGP", "currency for {tpl}");
                assert_eq!(fields.account.as_deref(), account, "account for {tpl}");
                assert_eq!(
                    fields.counterparty.as_deref(),
                    counterparty,
                    "counterparty for {tpl}"
                );
                assert_eq!(
                    fields.reference.as_deref(),
                    reference,
                    "reference for {tpl}"
                );
                assert_eq!(
                    fields
                        .occurred_at
                        .with_timezone(&Cairo)
                        .format("%Y-%m-%d %H:%M")
                        .to_string(),
                    occurred,
                    "occurred_at for {tpl}"
                );
                seen.insert(template);
            }
            other => panic!("{tpl}: expected a match, got {} for {body}", other.status()),
        }
    }
    let v3: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM banksms.parse_templates WHERE priority >= 50 ORDER BY priority",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    for name in &v3 {
        assert!(seen.contains(name), "no case exercises template {name}");
    }

    // PetroApp through the Banque Misr wording is still suppressed.
    let petro = "تم تنفيذ تحويل لحظي من حسابكم رقم xxx6666 بمبلغ 60000 جم إلى petroapp cib رقم مرجعي 772000000082 يوم 2026-10-05 الساعه 11:44 للمزيد اتصل ب 19888 ";
    match parser::parse(petro, &templates, Some(at("2026-10-05 11:45"))) {
        Verdict::Suppressed { template } => assert_eq!(template, "wallet_transfer_v2"),
        other => panic!("petroapp must suppress, got {}", other.status()),
    }

    // Money-shaped messages that are not money movements stay ignored.
    for body in [
        "تم رفض المعاملة على بطاقتك المنتهية بـ **4444 بقيمة 0.00 EGP نظرا لادخال خاطئ لكود التحقق المدون خلف البطاقة",
        "نأسف لعدم إتمام المعاملة بمبلغ 640 EGP التي تمت باستخدم البطاقة رقم 7777 بسبب عدم كفاية الرصيد, المتاح 512.10EGP للمزيد اتصل ب 19623",
        "Your ABK-Egypt Credit Card 5100***3434 monthly statement has a balance of EGP 3120.40 /Minimum Due Payment is EGP 640.00 to be paid before Due Date: 28/10/26 by 5:00PM maximum.",
        "عميلنا العزيز لانتظام حسابكم يرجى سداد قسط شهر10بمبلغ45000جم في ميعاد الاستحقاق05-10عن طريق الايداع او اجراء تحويل الى حسابكم الجاري طرفنا .",
        "هذا الكود سري يرجى عدم مشاركته مع اي شخص اخر وهو مخصص لعملية الشراء التي تقوم بها الان بمبلغ EGP 39.99 عند التاجرGoogle بالبطاقة المنتهية برقم7777 NBE OTP: 123456",
    ] {
        let verdict = parser::parse(body, &templates, Some(at("2026-10-01 12:00")));
        assert!(matches!(verdict, Verdict::Ignored), "must stay ignored: {body}");
    }
}

/* ------------------------------------------------------------------------ */
/* Split lifecycle: strict sums, per-part loans, guards, unsplit.            */
/* ------------------------------------------------------------------------ */

#[actix_web::test]
async fn split_lifecycle_keeps_the_money_exact() {
    use actix_web::{test, web, App};

    support::init();
    let pool = support::fresh_db("apex_bsms_split").await;
    apex::boot::run_banksms_migrations(&pool).await.unwrap();
    sqlx::raw_sql("INSERT INTO public.employees (id, name) VALUES (12, 'عماد جرجس');")
        .execute(&pool)
        .await
        .unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .configure(apex::api::configure),
    )
    .await;
    let auth = ("Authorization", format!("Bearer {}", support::admin_token(3)));

    // A 20,000 transfer that actually covers an advance and parts.
    let req = test::TestRequest::post()
        .uri("/api/v1/transactions")
        .insert_header(auth.clone())
        .set_json(serde_json::json!({
            "direction": "out", "amount": "20000", "occurred_at": "2026-08-12T10:00:00Z",
            "counterparty": "رمزي ط... ك... ذ..."
        }))
        .to_request();
    let parent: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    let pid = parent["id"].as_i64().unwrap();

    // Parts that don't add up are refused with the amounts in the message.
    let req = test::TestRequest::post()
        .uri(&format!("/api/v1/transactions/{pid}/split"))
        .insert_header(auth.clone())
        .insert_header(("If-Match", "1"))
        .set_json(serde_json::json!({ "parts": [
            { "amount": "15000", "category": "Advance", "employee_id": 12 },
            { "amount": "4000", "category": "Parts" }
        ]}))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 400, "wrong sum must refuse");

    // The exact split works and registers the advance part's loan.
    let req = test::TestRequest::post()
        .uri(&format!("/api/v1/transactions/{pid}/split"))
        .insert_header(auth.clone())
        .insert_header(("If-Match", "1"))
        .set_json(serde_json::json!({ "parts": [
            { "amount": "15000", "category": "Advance", "employee_id": 12 },
            { "amount": "5000", "category": "Parts" }
        ]}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let set: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(set["parts"].as_array().unwrap().len(), 2);
    let advance_child = set["parts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["category"] == "Advance")
        .unwrap();
    let loan_id = advance_child["loan"]["id"].as_i64().expect("advance part registers");
    let child_id = advance_child["id"].as_i64().unwrap();
    let loan_amount: f64 = sqlx::query_scalar("SELECT amount FROM public.loans WHERE id = $1")
        .bind(loan_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(loan_amount, 15000.0);

    // The ledger shows the parts, never the split container.
    let req = test::TestRequest::get()
        .uri("/api/v1/transactions?direction=out")
        .insert_header(auth.clone())
        .to_request();
    let page: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    let ids: Vec<i64> = page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_i64().unwrap())
        .collect();
    assert!(!ids.contains(&pid), "split parent must leave the ledger");
    assert!(ids.contains(&child_id), "split parts are ledger rows");

    // Child money fields are the split's invariant.
    let req = test::TestRequest::patch()
        .uri(&format!("/api/v1/transactions/{child_id}"))
        .insert_header(auth.clone())
        .insert_header(("If-Match", "1"))
        .set_json(serde_json::json!({ "amount": "14000" }))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 400);
    // ...but its meaning stays editable.
    let req = test::TestRequest::patch()
        .uri(&format!("/api/v1/transactions/{child_id}"))
        .insert_header(auth.clone())
        .insert_header(("If-Match", "1"))
        .set_json(serde_json::json!({ "description": "سلفة أغسطس" }))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 200);

    // Unsplit restores the parent and unregisters the unpaid loan.
    let req = test::TestRequest::post()
        .uri(&format!("/api/v1/transactions/{pid}/unsplit"))
        .insert_header(auth.clone())
        .insert_header(("If-Match", "2"))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 200);
    let loan_gone: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT deleted_at FROM public.loans WHERE id = $1")
            .bind(loan_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(loan_gone.is_some(), "unsplit unregisters the part's loan");
    let live_children: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM banksms.transactions WHERE parent_id = $1 AND deleted_at IS NULL",
    )
    .bind(pid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(live_children, 0);

    // A settled loan on any part locks the split, same as everywhere else.
    let req = test::TestRequest::post()
        .uri(&format!("/api/v1/transactions/{pid}/split"))
        .insert_header(auth.clone())
        .insert_header(("If-Match", "3"))
        .set_json(serde_json::json!({ "parts": [
            { "amount": "18000", "category": "Advance", "employee_id": 12 },
            { "amount": "2000", "category": "Other" }
        ]}))
        .to_request();
    let set: serde_json::Value = test::call_and_read_body_json(&app, {
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        test::TestRequest::get()
            .uri(&format!("/api/v1/transactions/{pid}/split"))
            .insert_header(auth.clone())
            .to_request()
    })
    .await;
    let new_loan = set["parts"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|p| p["loan"]["id"].as_i64())
        .unwrap();
    sqlx::query("UPDATE public.loans SET is_paid = true WHERE id = $1")
        .bind(new_loan)
        .execute(&pool)
        .await
        .unwrap();
    let req = test::TestRequest::post()
        .uri(&format!("/api/v1/transactions/{pid}/unsplit"))
        .insert_header(auth.clone())
        .insert_header(("If-Match", "4"))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        409,
        "a settled loan blocks the unsplit"
    );
}

#[actix_web::test]
async fn ledger_sorts_by_amount_across_pages() {
    use actix_web::{test, web, App};

    support::init();
    let pool = support::fresh_db("apex_bsms_sort_amount").await;
    apex::boot::run_banksms_migrations(&pool).await.unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .configure(apex::api::configure),
    )
    .await;
    let auth = ("Authorization", format!("Bearer {}", support::admin_token(3)));

    // Amounts deliberately out of date order, with a tie to exercise the id
    // tiebreak in the keyset.
    for (amount, at) in [
        ("150", "2026-08-01T10:00:00Z"),
        ("9000.5", "2026-08-02T10:00:00Z"),
        ("42", "2026-08-03T10:00:00Z"),
        ("1200", "2026-08-04T10:00:00Z"),
        ("1200", "2026-08-05T10:00:00Z"),
    ] {
        let req = test::TestRequest::post()
            .uri("/api/v1/transactions")
            .insert_header(auth.clone())
            .set_json(serde_json::json!({
                "direction": "out", "amount": amount, "occurred_at": at,
            }))
            .to_request();
        assert!(test::call_service(&app, req).await.status().is_success());
    }

    // Page through two at a time; the cursor must carry the amount order.
    let mut amounts: Vec<f64> = Vec::new();
    let mut ids: Vec<i64> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut uri = "/api/v1/transactions?direction=out&sort=amount&limit=2".to_string();
        if let Some(c) = &cursor {
            uri.push_str(&format!("&cursor={}", urlencoding_min(c)));
        }
        let req = test::TestRequest::get()
            .uri(&uri)
            .insert_header(auth.clone())
            .to_request();
        let page: serde_json::Value = test::call_and_read_body_json(&app, req).await;
        for row in page["data"].as_array().unwrap() {
            amounts.push(row["amount"].as_str().unwrap().parse().unwrap());
            ids.push(row["id"].as_i64().unwrap());
        }
        match page["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    assert_eq!(amounts, vec![9000.5, 1200.0, 1200.0, 150.0, 42.0]);
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 5, "no row repeats or goes missing across pages");

    // The work queue: once a row has a category it leaves the uncategorized
    // filter, on the list and on the statistics alike.
    sqlx::query("UPDATE banksms.transactions SET category = 'Parts' WHERE amount = 9000.5")
        .execute(&pool)
        .await
        .unwrap();
    let req = test::TestRequest::get()
        .uri("/api/v1/transactions?direction=out&sort=amount&category=__uncategorized__")
        .insert_header(auth.clone())
        .to_request();
    let page: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    let queue: Vec<f64> = page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["amount"].as_str().unwrap().parse().unwrap())
        .collect();
    assert_eq!(queue, vec![1200.0, 1200.0, 150.0, 42.0]);
    let req = test::TestRequest::get()
        .uri("/api/v1/transactions/statistics?category=__uncategorized__")
        .insert_header(auth.clone())
        .to_request();
    let stats: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(stats["count"], 4);

    // A date cursor replayed under the amount sort is refused, not misread.
    let req = test::TestRequest::get()
        .uri("/api/v1/transactions?sort=amount&cursor=1754042400000:1")
        .insert_header(auth.clone())
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 400);

    // Unknown sort keys are refused.
    let req = test::TestRequest::get()
        .uri("/api/v1/transactions?sort=bogus")
        .insert_header(auth.clone())
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 400);
}

/// Cursors are `amt:<decimal>:<id>` — only ':' and '.' need care in a query.
fn urlencoding_min(s: &str) -> String {
    s.replace(':', "%3A")
}
