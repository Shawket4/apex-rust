//! Reference data for the forms: categories, parties (drivers + employees),
//! vehicles. All read-only over HTTP — categories are edited via psql (12
//! rows), employees are created through FalconGo's own endpoint (it owns
//! `public.employees` and stays the single writer).

use actix_web::{web, HttpResponse};
use serde::Serialize;
use sqlx::{PgPool, Row};

use crate::errors::AppResult;

#[derive(Debug, Serialize)]
pub struct Category {
    pub id: i64,
    pub key: String,
    pub label: String,
    pub label_ar: String,
    pub posting_kind: Option<String>,
    pub required_party: String,
    pub sort_order: i32,
    pub enabled: bool,
}

pub async fn list_categories(pool: web::Data<PgPool>) -> AppResult<HttpResponse> {
    let rows = sqlx::query(
        "SELECT id, key, label, label_ar, posting_kind, required_party, sort_order, enabled
         FROM banksms.categories WHERE enabled ORDER BY sort_order, id",
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
            posting_kind: r.get("posting_kind"),
            required_party: r.get("required_party"),
            sort_order: r.get("sort_order"),
            enabled: r.get("enabled"),
        })
        .collect();
    Ok(HttpResponse::Ok().json(out))
}

#[derive(Debug, Serialize)]
pub struct Party {
    pub id: i64,
    pub name: String,
    pub kind: &'static str,
    pub mobile_number: Option<String>,
}

/// Drivers and employees in one call — the picker needs both, and merging
/// client-side buys nothing. Stored by id, never by name: free-text names
/// decay into five spellings of one person and can't be reported on.
pub async fn list_parties(pool: web::Data<PgPool>) -> AppResult<HttpResponse> {
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
    pub car_no_plate: String,
}

pub async fn list_vehicles(pool: web::Data<PgPool>) -> AppResult<HttpResponse> {
    let rows = sqlx::query(
        "SELECT id::bigint AS id, COALESCE(car_no_plate, '') AS car_no_plate
         FROM public.cars
         WHERE deleted_at IS NULL AND COALESCE(car_no_plate, '') <> ''
         ORDER BY car_no_plate",
    )
    .fetch_all(pool.get_ref())
    .await?;

    let out: Vec<Vehicle> = rows
        .iter()
        .map(|r| Vehicle {
            id: r.get("id"),
            car_no_plate: r.get("car_no_plate"),
        })
        .collect();
    Ok(HttpResponse::Ok().json(out))
}
