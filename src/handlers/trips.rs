//! `GET /api/v1/trips` — the trips list, with revenue.
//!
//! Gated at permission 4. That is stricter than the statistics endpoint next to
//! it, which opens financial columns at 3, and the difference is deliberate:
//! statistics show revenue in aggregate, while this endpoint puts a figure
//! against an individual driver's individual trip. Those are different
//! disclosures and the stricter one wins.
//!
//! Permission is enforced twice on purpose. The route's `JwtAuth` refuses the
//! request outright, and [`crate::db::trip_queries::list_trips`] independently
//! decides whether to read the money columns at all — so if this endpoint is
//! ever remounted at a lower level, the failure is a missing field rather than
//! a silent leak of every driver's earnings.

use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use serde::Deserialize;
use sqlx::PgPool;

use crate::auth::Claims;
use crate::db::trip_queries::{list_trips, TripListFilters, DEFAULT_LIMIT};
use crate::models::trip_list::{TripListMeta, TripListResponse};

/// The permission that may see per-trip revenue.
const FINANCIAL_PERMISSION: i32 = 4;

#[derive(Debug, Deserialize)]
pub struct TripListQuery {
    pub page: Option<i64>,
    pub limit: Option<i64>,
    pub search: Option<String>,
    pub missing_data: Option<String>,
    pub receipt_status: Option<String>,
    pub company: Option<String>,
    /// `YYYY-MM-DD`, inclusive. Also scopes the revenue window — see
    /// [`crate::db::revenue::allocation`].
    pub from: Option<String>,
    pub to: Option<String>,
}

/// Empty query strings arrive as `Some("")` from the dashboard's form state.
/// Treating those as filters would match nothing at all, so they are dropped.
fn present(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

pub async fn get_trips(
    pool: web::Data<PgPool>,
    query: web::Query<TripListQuery>,
    req: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    let permission = req
        .extensions()
        .get::<Claims>()
        .and_then(|c| c.permission)
        .unwrap_or(0);
    let financial = permission >= FINANCIAL_PERMISSION;

    let query = query.into_inner();
    let filters = TripListFilters {
        page: query.page.unwrap_or(1),
        limit: query.limit.unwrap_or(DEFAULT_LIMIT),
        search: present(query.search),
        missing_data: present(query.missing_data),
        receipt_status: present(query.receipt_status),
        company: present(query.company),
        from: present(query.from),
        to: present(query.to),
    }
    .normalized();

    let (data, total) = list_trips(pool.get_ref(), &filters, financial)
        .await
        .map_err(|e| {
            log::error!("trips list failed: {e:#}");
            actix_web::error::ErrorInternalServerError("failed to fetch trips")
        })?;

    Ok(HttpResponse::Ok().json(TripListResponse {
        message: "Trips retrieved successfully",
        data,
        meta: TripListMeta {
            total,
            page: filters.page,
            limit: filters.limit,
            // Ceiling division. `limit` is clamped to at least 1 by
            // `normalized`, so this cannot divide by zero.
            pages: (total + filters.limit - 1) / filters.limit,
        },
    }))
}
