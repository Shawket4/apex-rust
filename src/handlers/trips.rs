//! `GET /api/v1/trips` — the trips list, with revenue.
//!
//! Two permission levels are in play, and conflating them breaks something
//! either way:
//!
//! * **Level 1 sees the list.** That is what FalconGo's route required, and
//!   dispatchers live on this page. Gating the whole endpoint higher would take
//!   the trips page away from everyone who does the work.
//! * **Level 4 sees the money.** Stricter than the statistics endpoint next
//!   door, which opens financial columns at 3, and deliberately so: statistics
//!   show revenue in aggregate, while this puts a figure against one driver's
//!   one trip. Those are different disclosures and the stricter one wins.
//!
//! The money gate is enforced in [`crate::db::trip_queries::list_trips`],
//! which decides from the caller's permission whether to read the columns at
//! all. So a caller below 4 does not receive figures that some layer above was
//! trusted to strip. Below 3 the fee mapping's raw `fee` is withheld as well
//! where it is a price (Petrol Arrows' EGP per 1,000 L); it used to reach every
//! level.

use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use serde::Deserialize;
use sqlx::PgPool;

use crate::auth::Claims;
use crate::db::trip_queries::{list_trips, TripListFilters, DEFAULT_LIMIT};
use crate::models::trip_list::{TripListMeta, TripListResponse};
use crate::utils::response;

#[derive(Debug, Deserialize)]
pub struct TripListQuery {
    pub page: Option<i64>,
    pub limit: Option<i64>,
    pub search: Option<String>,
    pub missing_data: Option<String>,
    pub receipt_status: Option<String>,
    pub company: Option<String>,
    /// `YYYY-MM-DD`, inclusive.
    pub from: Option<String>,
    pub to: Option<String>,
    /// `msgpack` to get MessagePack instead of JSON. The dashboard always asks
    /// for it: these payloads nest receipt steps, parent headers and scanned
    /// receipts under every row, and the map-keyed encoding is several times
    /// smaller on the wire for that shape.
    pub format: Option<String>,
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

    let query = query.into_inner();
    let use_msgpack = query.format.as_deref() == Some("msgpack");
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

    let (data, total) = list_trips(pool.get_ref(), &filters, permission)
        .await
        .map_err(|e| {
            log::error!("trips list failed: {e:#}");
            actix_web::error::ErrorInternalServerError("failed to fetch trips")
        })?;

    let payload = TripListResponse {
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
    };

    response(&payload, use_msgpack).map_err(actix_web::error::ErrorInternalServerError)
}


/// The permission needed to see the list at all, matching the FalconGo route
/// this replaces. Money is gated separately at
/// [`crate::db::trip_queries::MONEY_PERMISSION`].
pub const VIEW_PERMISSION: i32 = 1;
