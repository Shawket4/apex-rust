//! Categories, and the people a transaction can be attributed to.
//!
//! Drivers and employees are read from FalconGo's tables. They are NOT written
//! here: `public.employees` already has full CRUD in FalconGo
//! (`POST /api/employees`), so the dashboard creates a new employee through that
//! endpoint and FalconGo remains the single writer. This module only reads, and
//! only so the picker can offer real rows with real ids.
//!
//! Why ids and not names: `public.loans.method` in this same database is what
//! happens without them. One person is stored there as 'Shady', 'shady',
//! 'شادى', 'Shady (تاني)' and 'Shady (x2)', and the column also accumulated
//! payment methods, reasons, dates and entire sentences. A free-text party is
//! unreportable within months.

use actix_web::{web, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};

use crate::api::{require_admin, require_read};
use crate::auth::JwtAuth;
use crate::errors::{AppError, AppResult};

fn guard() -> JwtAuth {
    JwtAuth {
        required_permission: None,
    }
}

/* -------------------------------------------------------------------------- */
/* Categories                                                                  */
/* -------------------------------------------------------------------------- */

#[derive(Debug, Serialize)]
pub struct Category {
    pub id: i64,
    pub key: String,
    pub label: String,
    pub label_ar: Option<String>,
    /// "none" | "loan" — what categorising into this posts downstream.
    pub posting_target: String,
    /// "none" | "driver" | "employee" | "either".
    pub required_party: String,
    pub field_spec: serde_json::Value,
    pub sort_order: i32,
}

async fn list_categories(pool: web::Data<PgPool>, req: HttpRequest) -> AppResult<HttpResponse> {
    require_read(&req)?;

    let rows = sqlx::query(
        "SELECT id, key, label, label_ar,
                posting_target::text AS posting_target,
                required_party::text AS required_party,
                field_spec, sort_order
         FROM banksms.categories
         WHERE enabled
         ORDER BY sort_order, label",
    )
    .fetch_all(pool.get_ref())
    .await?;

    let out: Vec<Category> = rows
        .iter()
        .map(|r| Category {
            id: r.get("id"),
            key: r.get("key"),
            label: r.get("label"),
            label_ar: r.get("label_ar"),
            posting_target: r.get("posting_target"),
            required_party: r.get("required_party"),
            field_spec: r.get("field_spec"),
            sort_order: r.get("sort_order"),
        })
        .collect();

    Ok(HttpResponse::Ok().json(out))
}

#[derive(Debug, Deserialize)]
pub struct UpsertCategory {
    pub key: String,
    pub label: String,
    pub label_ar: Option<String>,
    pub posting_target: Option<String>,
    pub required_party: Option<String>,
    pub sort_order: Option<i32>,
}

async fn create_category(
    pool: web::Data<PgPool>,
    body: web::Json<UpsertCategory>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    let ctx = require_admin(&req)?;
    let c = body.into_inner();

    if c.key.trim().is_empty() || c.label.trim().is_empty() {
        return Err(AppError::BadRequest("key and label are required".into()));
    }

    let posting = c.posting_target.as_deref().unwrap_or("none");
    let party = c.required_party.as_deref().unwrap_or("none");
    if !matches!(posting, "none" | "loan") {
        return Err(AppError::BadRequest("posting_target must be 'none' or 'loan'".into()));
    }
    if !matches!(party, "none" | "driver" | "employee" | "either") {
        return Err(AppError::BadRequest(
            "required_party must be none, driver, employee or either".into(),
        ));
    }
    // Mirrors the CHECK constraint, so the error is a helpful 400 rather than a
    // constraint violation surfacing as a 409.
    if posting != "none" && party == "none" {
        return Err(AppError::BadRequest(
            "a category that posts to loans must require a driver or employee".into(),
        ));
    }

    let row = sqlx::query(
        "INSERT INTO banksms.categories
             (key, label, label_ar, posting_target, required_party, sort_order, created_by)
         VALUES ($1, $2, $3, $4::text::banksms.posting_target,
                 $5::text::banksms.party_kind, COALESCE($6, 100), $7)
         RETURNING id",
    )
    .bind(c.key.trim())
    .bind(c.label.trim())
    .bind(c.label_ar.as_deref())
    .bind(posting)
    .bind(party)
    .bind(c.sort_order)
    .bind(ctx.actor())
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Created().json(serde_json::json!({ "id": row.get::<i64, _>("id") })))
}

/* -------------------------------------------------------------------------- */
/* Parties                                                                     */
/* -------------------------------------------------------------------------- */

#[derive(Debug, Serialize)]
pub struct Party {
    pub id: i64,
    pub name: String,
    pub kind: &'static str,
    pub mobile_number: Option<String>,
}

/// Drivers and employees in one call — the picker needs both and firing two
/// requests just to merge them client-side buys nothing.
async fn list_parties(pool: web::Data<PgPool>, req: HttpRequest) -> AppResult<HttpResponse> {
    require_read(&req)?;

    let drivers = sqlx::query(
        "SELECT id::bigint AS id, COALESCE(name, '') AS name, mobile_number
         FROM public.drivers
         WHERE deleted_at IS NULL AND COALESCE(name, '') <> ''
         ORDER BY name",
    )
    .fetch_all(pool.get_ref())
    .await?;

    let employees = sqlx::query(
        "SELECT id::bigint AS id, COALESCE(name, '') AS name, mobile_number
         FROM public.employees
         WHERE deleted_at IS NULL AND COALESCE(is_active, true)
           AND COALESCE(name, '') <> ''
         ORDER BY name",
    )
    .fetch_all(pool.get_ref())
    .await?;

    let mut out: Vec<Party> = Vec::with_capacity(drivers.len() + employees.len());
    for r in &drivers {
        out.push(Party {
            id: r.get("id"),
            name: r.get("name"),
            kind: "driver",
            mobile_number: r.get("mobile_number"),
        });
    }
    for r in &employees {
        out.push(Party {
            id: r.get("id"),
            name: r.get("name"),
            kind: "employee",
            mobile_number: r.get("mobile_number"),
        });
    }

    Ok(HttpResponse::Ok().json(out))
}

#[derive(Debug, Serialize)]
pub struct Vehicle {
    pub id: i64,
    pub plate: String,
}

/// Vehicles for the transaction form's dropdown.
///
/// Stored by id on the transaction, not by plate. A plate typed by hand is the
/// same failure mode as a typed person: `ف ج م 8567` and `ف ج م  8567` are one
/// vehicle to a human and two to a report.
async fn list_vehicles(pool: web::Data<PgPool>, req: HttpRequest) -> AppResult<HttpResponse> {
    require_read(&req)?;

    let rows = sqlx::query(
        "SELECT id::bigint AS id, COALESCE(car_no_plate, '') AS plate
         FROM public.cars
         WHERE deleted_at IS NULL AND COALESCE(car_no_plate, '') <> ''
         ORDER BY car_no_plate",
    )
    .fetch_all(pool.get_ref())
    .await?;

    let out: Vec<Vehicle> = rows
        .iter()
        .map(|r| Vehicle { id: r.get("id"), plate: r.get("plate") })
        .collect();

    Ok(HttpResponse::Ok().json(out))
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api/v1/categories")
            .route("", web::get().to(list_categories).wrap(guard()))
            .route("", web::post().to(create_category).wrap(guard())),
    )
    .service(web::scope("/api/v1/parties").route("", web::get().to(list_parties).wrap(guard())))
    .service(web::scope("/api/v1/vehicles").route("", web::get().to(list_vehicles).wrap(guard())));
}
