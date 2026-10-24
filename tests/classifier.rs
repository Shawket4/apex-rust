//! The classifier end to end: payee import, a run over a mock ledger, the
//! review flow through the API (accept, correct, dismiss), and the money:
//! excluded categories out of every total, a linked advance counted once.

mod support;

use actix_web::{test, web, App};
use rust_decimal::Decimal;
use sqlx::Row;
use std::str::FromStr;

const SEED: &str = r#"
INSERT INTO public.drivers (id, name) VALUES (22, 'حسام فوزي'), (16, 'مينا عادل جرجس فهيم');
INSERT INTO public.loans (id, driver_id, amount, method, date, kind)
VALUES (500, 22, 1500, 'Kareem', '2026-08-25', 'advance'),
       (501, 16, 2800, 'Kareem', '2026-08-27', 'advance');

INSERT INTO banksms.raw_messages (id, wa_message_id, chat_jid, wa_timestamp, body, status, template) VALUES
 (1, 'W1', 'x', '2026-08-13 09:00Z', 'mock', 'matched', 'arabic_ipn'),
 (2, 'W2', 'x', '2026-08-25 08:00Z', 'mock', 'matched', 'wallet_transfer'),
 (3, 'W3', 'x', '2026-08-20 10:05Z', 'mock', 'matched', 'arabic_ipn'),
 (4, 'W4', 'x', '2026-08-20 10:07Z', 'mock', 'matched', 'bm_transfer_in'),
 (5, 'W5', 'x', '2026-08-13 15:34Z', 'mock', 'matched', 'arabic_ipn'),
 (6, 'W6', 'x', '2026-08-13 15:40Z', 'mock', 'matched', 'ipn_refund'),
 (7, 'W7', 'x', '2026-08-28 09:00Z', 'mock', 'matched', 'wallet_transfer'),
 (8, 'W8', 'x', '2026-08-29 09:00Z', 'mock', 'matched', 'instant_transfer'),
 (9, 'W9', 'x', '2026-08-20 10:08Z', 'mock', 'matched', 'bm_transfer_out'),
 (10, 'W10', 'x', '2026-10-07 06:36Z',
  'تم تنفيذ تحويل لحظي من حسابكم رقم xxx6666 بمبلغ 60000 جم إلى petroapp cib رقم مرجعي 772000000082 يوم 2026-10-05 الساعه 11:44 للمزيد اتصل ب 19888',
  'suppressed', 'wallet_transfer_v2');

INSERT INTO banksms.transactions
  (id, source, raw_message_id, direction, amount, occurred_at, account, counterparty, reference, created_by) VALUES
 (1, 'whatsapp', 1, 'out', 180000, '2026-08-13 09:00Z', '2222', 'tirecorp', 'FT9', 'parser'),
 (2, 'whatsapp', 2, 'out', 1500, '2026-08-25 08:00Z', '5555', 'حسام ف*** ر***', '1', 'parser'),
 (3, 'whatsapp', 3, 'out', 95000, '2026-08-20 10:05Z', '2222', 'Kareem', 'FT3', 'parser'),
 (4, 'whatsapp', 4, 'in', 95000, '2026-08-20 10:07Z', '6666', NULL, NULL, 'parser'),
 (5, 'whatsapp', 5, 'out', 850, '2026-08-13 15:34Z', '2222', 'gadget shop', 'FT26225ABCDE', 'parser'),
 (6, 'whatsapp', 6, 'in', 850, '2026-08-13 15:34Z', '2222', 'gadget shop', 'FT26225ABCDE', 'parser'),
 (7, 'whatsapp', 7, 'out', 3500, '2026-08-28 09:00Z', '5555', 'مينا ع... ج... ف..', '2', 'parser'),
 (8, 'whatsapp', 8, 'out', 5000, '2026-08-29 09:00Z', '1111', NULL, 'd23b28a2', 'parser'),
 (9, 'whatsapp', 9, 'out', 60000, '2026-08-20 10:08Z', '6666', NULL, NULL, 'parser');
SELECT setval(pg_get_serial_sequence('banksms.transactions', 'id'), 100);
"#;

const PAYEES: &str = r#"{"payees": [
  {"name": "Tire Corp", "kind": "supplier", "category": "Tires", "consistency": "always", "aliases": ["tirecorp"]},
  {"name": "Kareem Samir", "kind": "owner", "category": "InternalTransfer", "consistency": "varies", "aliases": ["Kareem", "KAREEM SAMIR HASSAN"]},
  {"name": "Mina Adel", "kind": "driver", "category": "DriverCustody", "consistency": "varies", "driver_id": 16, "aliases": ["مينا ع... ج... ف.."]}
]}"#;

async fn txn(pool: &sqlx::PgPool, id: i64) -> sqlx::postgres::PgRow {
    sqlx::query(
        "SELECT t.category, t.driver_id, t.linked_loan_id, t.loan_id, t.version, c.status, c.rule
         FROM banksms.transactions t
         LEFT JOIN banksms.classifications c ON c.transaction_id = t.id WHERE t.id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[actix_web::test]
async fn classifier_end_to_end() {
    support::init();
    let pool = support::fresh_db("apex_classifier").await;
    apex::boot::run_banksms_migrations(&pool).await.unwrap();
    sqlx::raw_sql(SEED).execute(&pool).await.unwrap();

    // Import twice: the second updates in place.
    let file: apex::classify::PayeeFile = serde_json::from_str(PAYEES).unwrap();
    assert_eq!(
        apex::classify::import_payees(&pool, &file, "test")
            .await
            .unwrap(),
        (3, 4)
    );
    apex::classify::import_payees(&pool, &file, "test")
        .await
        .unwrap();
    let payees: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM banksms.payees")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(payees, 3);
    let bad: apex::classify::PayeeFile = serde_json::from_str(
        r#"{"payees": [{"name": "X", "kind": "supplier", "category": "NoSuchThing", "aliases": ["x"]}]}"#,
    )
    .unwrap();
    assert!(apex::classify::import_payees(&pool, &bad, "test")
        .await
        .is_err());

    // A dry run writes nothing.
    let dry = apex::classify::run(&pool, None, true).await.unwrap();
    assert!(dry.applied > 0);
    let written: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM banksms.classifications")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(written, 0);

    let report = apex::classify::run(&pool, None, false).await.unwrap();
    for d in &report.decisions {
        println!("{} {}", d.apply, apex::classify::describe(d));
    }

    let r = txn(&pool, 1).await;
    assert_eq!(
        r.get::<Option<String>, _>("category").as_deref(),
        Some("Tires")
    );
    assert_eq!(
        r.get::<Option<String>, _>("status").as_deref(),
        Some("applied")
    );

    // The advance FalconGo already has: linked, never registered again.
    let r = txn(&pool, 2).await;
    assert_eq!(
        r.get::<Option<String>, _>("category").as_deref(),
        Some("Advance")
    );
    assert_eq!(r.get::<Option<i64>, _>("driver_id"), Some(22));
    assert_eq!(r.get::<Option<i64>, _>("linked_loan_id"), Some(500));
    assert_eq!(r.get::<Option<i64>, _>("loan_id"), None);

    for id in [3, 4] {
        let r = txn(&pool, id).await;
        assert_eq!(
            r.get::<Option<String>, _>("category").as_deref(),
            Some("InternalTransfer"),
            "txn {id}"
        );
    }
    for id in [5, 6] {
        let r = txn(&pool, id).await;
        assert_eq!(
            r.get::<Option<String>, _>("category").as_deref(),
            Some("Reversed"),
            "txn {id}"
        );
    }
    // Varies: the person from the label is filled in, the category waits.
    let r = txn(&pool, 7).await;
    assert_eq!(r.get::<Option<String>, _>("category"), None);
    assert_eq!(r.get::<Option<i64>, _>("driver_id"), Some(16));
    assert_eq!(
        r.get::<Option<String>, _>("status").as_deref(),
        Some("suggested")
    );
    // Nothing to go on.
    assert_eq!(txn(&pool, 8).await.get::<Option<String>, _>("status"), None);
    // Unnamed 60,000 from an account that tops up PetroApp: asked, not applied.
    let r = txn(&pool, 9).await;
    assert_eq!(r.get::<Option<String>, _>("category"), None);
    assert_eq!(
        r.get::<Option<String>, _>("rule").as_deref(),
        Some("petroapp_topup")
    );

    // A second run changes nothing that was applied.
    let again = apex::classify::run(&pool, None, false).await.unwrap();
    assert_eq!(again.applied, 0);

    // A refund that arrives later replaces the classifier's own decision,
    // through the poller's path (only the new transaction is a target).
    sqlx::raw_sql(
        "INSERT INTO banksms.raw_messages (id, wa_message_id, chat_jid, wa_timestamp, body, status, template)
         VALUES (11, 'W11', 'x', '2026-09-02 10:00Z', 'mock', 'matched', 'arabic_ipn');
         INSERT INTO banksms.transactions
           (id, source, raw_message_id, direction, amount, occurred_at, account, counterparty, reference, created_by)
         VALUES (10, 'whatsapp', 11, 'out', 50000, '2026-09-02 10:00Z', '2222', 'tirecorp', 'FT10', 'parser');",
    )
    .execute(&pool)
    .await
    .unwrap();
    apex::classify::run(&pool, Some(&[10]), false)
        .await
        .unwrap();
    assert_eq!(
        txn(&pool, 10)
            .await
            .get::<Option<String>, _>("category")
            .as_deref(),
        Some("Tires")
    );
    sqlx::raw_sql(
        "INSERT INTO banksms.raw_messages (id, wa_message_id, chat_jid, wa_timestamp, body, status, template)
         VALUES (12, 'W12', 'x', '2026-09-02 10:20Z', 'mock', 'matched', 'ipn_refund');
         INSERT INTO banksms.transactions
           (id, source, raw_message_id, direction, amount, occurred_at, account, counterparty, reference, created_by)
         VALUES (11, 'whatsapp', 12, 'in', 50000, '2026-09-02 10:00Z', '2222', 'tirecorp', 'FT10', 'parser');",
    )
    .execute(&pool)
    .await
    .unwrap();
    apex::classify::run(&pool, Some(&[11]), false)
        .await
        .unwrap();
    for id in [10, 11] {
        let r = txn(&pool, id).await;
        assert_eq!(
            r.get::<Option<String>, _>("category").as_deref(),
            Some("Reversed"),
            "txn {id}"
        );
        assert_eq!(
            r.get::<Option<String>, _>("status").as_deref(),
            Some("applied")
        );
    }
    assert_eq!(
        txn(&pool, 10)
            .await
            .get::<Option<String>, _>("rule")
            .as_deref(),
        Some("refunded")
    );

    /* ------------------------------ the money ----------------------------- */

    use apex::db::dashboard_queries as q;
    let bank = q::cash_out_total(&pool, "2026-08-01", "2026-08-31")
        .await
        .unwrap();
    // 180000 tires + 1500 advance + 3500 + 5000 + 60000; not the 95000
    // between own accounts, not the refunded 850.
    assert_eq!(bank, Decimal::from_str("250000").unwrap());
    let advances = q::advances_issued(&pool, "2026-08-01", "2026-08-31")
        .await
        .unwrap();
    assert_eq!(
        advances, 2800.0,
        "the linked 2000 counts once, as the bank row"
    );

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .configure(apex::api::configure),
    )
    .await;
    let auth = (
        "Authorization",
        format!("Bearer {}", support::admin_token(7)),
    );
    let get = |uri: String| {
        test::TestRequest::get()
            .uri(&uri)
            .insert_header(auth.clone())
            .to_request()
    };

    let stats: serde_json::Value = test::call_and_read_body_json(
        &app,
        get("/api/v1/transactions/statistics?from=2026-08-01T00:00:00Z&to=2026-09-01T00:00:00Z&include_fuel=false".into()),
    )
    .await;
    // Bank 250000 + the FalconGo advance of 2800 (loan 501); loan 500 shows
    // as its bank payment only.
    assert_eq!(
        Decimal::from_str(stats["total_out"].as_str().unwrap()).unwrap(),
        Decimal::from(252800)
    );
    assert_eq!(
        Decimal::from_str(stats["excluded"]["total"].as_str().unwrap()).unwrap(),
        Decimal::from(95850)
    );
    assert_eq!(
        stats["pending_in"]["count"], 0,
        "categorised incoming rows are decided"
    );

    let page: serde_json::Value = test::call_and_read_body_json(
        &app,
        get("/api/v1/transactions?from=2026-08-01T00:00:00Z&to=2026-09-01T00:00:00Z".into()),
    )
    .await;
    let ids: Vec<i64> = page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert!(
        !ids.contains(&(2_000_000_000_000 + 500)),
        "linked loan hidden: {ids:?}"
    );
    assert!(ids.contains(&(2_000_000_000_000 + 501)));

    let review: serde_json::Value =
        test::call_and_read_body_json(&app, get("/api/v1/transactions?review=true".into())).await;
    let rows = review["data"].as_array().unwrap();
    let ids: Vec<i64> = rows.iter().map(|r| r["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, vec![7, 9], "newest first");
    assert_eq!(rows[0]["classification"]["status"], "suggested");
    assert_eq!(rows[0]["classification"]["category"], "DriverCustody");
    assert!(rows[0]["classification"]["reason"]
        .as_str()
        .unwrap()
        .contains("Mina Adel"));

    let detail: serde_json::Value =
        test::call_and_read_body_json(&app, get("/api/v1/transactions/2".into())).await;
    assert_eq!(detail["linked_loan_id"], 500);
    assert_eq!(detail["classification"]["rule"], "advance_link");

    let patch = |id: i64, version: i32, body: serde_json::Value| {
        test::TestRequest::patch()
            .uri(&format!("/api/v1/transactions/{id}"))
            .insert_header(auth.clone())
            .insert_header(("If-Match", version.to_string()))
            .set_json(body)
            .to_request()
    };
    let version = |id: i64| {
        let pool = pool.clone();
        async move { txn(&pool, id).await.get::<i32, _>("version") }
    };

    /* ---------------------------- the review ------------------------------ */

    // Taking the suggestion.
    let resp = test::call_service(
        &app,
        patch(
            7,
            version(7).await,
            serde_json::json!({"category": "DriverCustody"}),
        ),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        txn(&pool, 7)
            .await
            .get::<Option<String>, _>("status")
            .as_deref(),
        Some("accepted")
    );

    // Correcting what was applied.
    let resp = test::call_service(
        &app,
        patch(
            1,
            version(1).await,
            serde_json::json!({"category": "Parts"}),
        ),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        txn(&pool, 1)
            .await
            .get::<Option<String>, _>("status")
            .as_deref(),
        Some("corrected")
    );

    // An edit that decides nothing (a description) leaves it applied.
    let resp = test::call_service(
        &app,
        patch(
            3,
            version(3).await,
            serde_json::json!({"description": "to Kareem"}),
        ),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        txn(&pool, 3)
            .await
            .get::<Option<String>, _>("status")
            .as_deref(),
        Some("applied")
    );

    // Not this; don't ask again.
    let req = test::TestRequest::post()
        .uri("/api/v1/transactions/9/suggestion/dismiss")
        .insert_header(auth.clone())
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 204);
    apex::classify::run(&pool, None, false).await.unwrap();
    assert_eq!(
        txn(&pool, 9)
            .await
            .get::<Option<String>, _>("status")
            .as_deref(),
        Some("dismissed")
    );
    let req = test::TestRequest::post()
        .uri("/api/v1/transactions/9/suggestion/dismiss")
        .insert_header(auth.clone())
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 404);

    /* --------------------------- linked advances -------------------------- */

    // Editing a linked advance never registers a second loan.
    let loans_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM public.loans")
        .fetch_one(&pool)
        .await
        .unwrap();
    let resp = test::call_service(
        &app,
        patch(
            2,
            version(2).await,
            serde_json::json!({"description": "advance"}),
        ),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let loans_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM public.loans")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(loans_before, loans_after);
    assert_eq!(txn(&pool, 2).await.get::<Option<i64>, _>("loan_id"), None);

    // A linked payment is not split.
    let req = test::TestRequest::post()
        .uri("/api/v1/transactions/2/split")
        .insert_header(auth.clone())
        .insert_header(("If-Match", version(2).await.to_string()))
        .set_json(serde_json::json!({"parts": [
            {"amount": "750", "category": "Other"}, {"amount": "750", "category": "Other"}]}))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 409);

    // Linking to a loan that is already another payment's is refused.
    let resp = test::call_service(
        &app,
        patch(
            8,
            version(8).await,
            serde_json::json!({"linked_loan_id": 500}),
        ),
    )
    .await;
    assert_eq!(resp.status(), 409);
    // So is a loan that does not exist.
    let resp = test::call_service(
        &app,
        patch(
            8,
            version(8).await,
            serde_json::json!({"linked_loan_id": 9999}),
        ),
    )
    .await;
    assert_eq!(resp.status(), 404);

    // Unlinking is a person's explicit call: now the Advance registers its
    // own loan, as any categorised advance does.
    let resp = test::call_service(
        &app,
        patch(
            2,
            version(2).await,
            serde_json::json!({"linked_loan_id": null}),
        ),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let r = txn(&pool, 2).await;
    assert_eq!(r.get::<Option<i64>, _>("linked_loan_id"), None);
    assert!(r.get::<Option<i64>, _>("loan_id").is_some());
    assert_eq!(
        r.get::<Option<String>, _>("status").as_deref(),
        Some("corrected")
    );

    // The categories endpoint says which groups are not summed.
    let cats: serde_json::Value =
        test::call_and_read_body_json(&app, get("/api/v1/categories".into())).await;
    let internal = cats
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["key"] == "InternalTransfer")
        .unwrap();
    assert_eq!(internal["report_group"], "excluded");

    let payees: serde_json::Value =
        test::call_and_read_body_json(&app, get("/api/v1/payees".into())).await;
    let owner = payees
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "Kareem Samir")
        .unwrap();
    assert_eq!(owner["spellings"].as_array().unwrap().len(), 2);
}

/// The loan sheet: imported whole (a bad file changes nothing), and a
/// transfer to the relative who pays the loans judged against it.
#[actix_web::test]
async fn loan_sheet_end_to_end() {
    support::init();
    let pool = support::fresh_db("apex_classifier_loans").await;
    apex::boot::run_banksms_migrations(&pool).await.unwrap();
    sqlx::raw_sql(
        r#"
        INSERT INTO banksms.raw_messages (id, wa_message_id, chat_jid, wa_timestamp, body, status, template) VALUES
         (1, 'L1', 'x', '2026-09-01 08:15Z', 'mock', 'matched', 'arabic_ipn'),
         (2, 'L2', 'x', '2026-09-01 08:25Z', 'mock', 'matched', 'arabic_ipn'),
         (3, 'L3', 'x', '2026-09-09 07:30Z', 'mock', 'matched', 'arabic_ipn');
        INSERT INTO banksms.transactions
          (id, source, raw_message_id, direction, amount, occurred_at, account, counterparty, reference, created_by) VALUES
         (1, 'whatsapp', 1, 'out', 120000, '2026-09-01 08:15Z', '2222', 'samira', 'FTL1', 'parser'),
         (2, 'whatsapp', 2, 'out', 95000, '2026-09-01 08:25Z', '2222', 'samira', 'FTL2', 'parser'),
         (3, 'whatsapp', 3, 'out', 12500, '2026-09-09 07:30Z', '2222', 'samira', 'FTL3', 'parser');
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();

    let payees: apex::classify::PayeeFile = serde_json::from_str(
        r#"{"payees": [
          {"name": "Samira Adel", "kind": "family", "category": "InternalTransfer", "consistency": "varies",
           "pays_loans": true, "aliases": ["samira"]},
          {"name": "Loan account", "kind": "bank", "category": "BankLoan", "consistency": "always",
           "aliases": ["loanacct"]}
        ]}"#,
    )
    .unwrap();
    apex::classify::import_payees(&pool, &payees, "test")
        .await
        .unwrap();
    let labelled = || async {
        sqlx::query_scalar::<_, chrono::DateTime<chrono::Utc>>(
            "SELECT updated_at FROM banksms.payees WHERE name = 'Samira Adel'",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let first = labelled().await;
    // The same file again leaves the label date alone.
    apex::classify::import_payees(&pool, &payees, "test")
        .await
        .unwrap();
    assert_eq!(labelled().await, first);

    let sheet: apex::classify::LoanFile = serde_json::from_str(
        r#"{"loans": [
          {"lender": "Bank A", "installment": "120000", "due_day": 1, "last_due": "2027-09-01"},
          {"lender": "Bank B", "name": "Bank B car loan", "installment": "64000", "due_day": 1, "last_due": "2028-01-01"},
          {"lender": "Bank C", "installment": "31000", "due_day": 3, "last_due": "2028-01-03"},
          {"lender": "Bank E", "installment": "80000", "due_day": 1, "last_due": "2029-01-01", "paid_via": "loanacct"}
        ]}"#,
    )
    .unwrap();
    assert_eq!(
        apex::classify::import_loans(&pool, &sheet, "test")
            .await
            .unwrap(),
        4
    );
    let via: Option<i64> =
        sqlx::query_scalar("SELECT payee_id FROM banksms.bank_loans WHERE lender = 'Bank E'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(via.is_some());

    // A bad file changes nothing.
    let bad: apex::classify::LoanFile = serde_json::from_str(
        r#"{"loans": [{"lender": "Bank Z", "installment": "1000", "due_day": 1, "last_due": "2027-01-01",
                       "paid_via": "nobody we know"}]}"#,
    )
    .unwrap();
    assert!(apex::classify::import_loans(&pool, &bad, "test")
        .await
        .is_err());
    let loans: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM banksms.bank_loans")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(loans, 4);

    apex::classify::run(&pool, None, false).await.unwrap();
    let got = |id: i64| {
        let pool = pool.clone();
        async move {
            let r = txn(&pool, id).await;
            (
                r.get::<Option<String>, _>("category"),
                r.get::<Option<String>, _>("status"),
                r.get::<Option<String>, _>("rule"),
            )
        }
    };
    // Exactly Bank A's installment, on its due date: written.
    assert_eq!(
        got(1).await,
        (
            Some("BankLoan".into()),
            Some("applied".into()),
            Some("loan_installment".into())
        )
    );
    // Bank B + Bank C: a loan payment, but which ones isn't certain: asked.
    assert_eq!(
        got(2).await,
        (
            None,
            Some("suggested".into()),
            Some("loan_installment".into())
        )
    );
    // No installment due around the 9th: savings, as the label says, asked.
    assert_eq!(
        got(3).await,
        (None, Some("suggested".into()), Some("payee".into()))
    );
    let reason: String =
        sqlx::query_scalar("SELECT reason FROM banksms.classifications WHERE transaction_id = 3")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(reason.contains("no loan installment"), "{reason}");
}
