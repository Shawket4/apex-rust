use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use anyhow::Result;
use sqlx::PgPool;

use crate::auth::Claims;
use crate::db::*;
use crate::models::*;
use crate::utils::response;

#[derive(serde::Deserialize)]
pub struct QueryParams {
    start_date: String,
    end_date: String,
    company: Option<String>,
    format: Option<String>,
}

pub async fn get_trip_statistics(
    pool: web::Data<PgPool>,
    query: web::Query<QueryParams>,
    req: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    // Get claims from request (guaranteed to exist since middleware validates admin)
    let claims = req.extensions().get::<Claims>().cloned();

    // Check if user has permission level >= 3 for financial access
    // Permission 3+ = Full financial data
    // Permission 1-2 = Limited data (no revenue/VAT/car rental)
    println!(
        "{:?}",
        claims.as_ref().and_then(|c| c.permission).unwrap_or(0)
    );
    let has_financial_access = claims
        .as_ref()
        .and_then(|c| c.permission)
        .map(|p| p >= 3)
        .unwrap_or(false);

    let use_msgpack = query.format.as_deref() == Some("msgpack");

    let companies = get_companies(
        pool.get_ref(),
        &query.start_date,
        &query.end_date,
        query.company.as_deref(),
    )
    .await
    .map_err(actix_web::error::ErrorInternalServerError)?;

    let mut statistics = Vec::new();

    for company in companies {
        let details = match company.as_str() {
            "Petrol Arrows" => get_petrol_arrows_stats(
                pool.get_ref(),
                &query.start_date,
                &query.end_date,
                has_financial_access,
            )
            .await
            .map_err(actix_web::error::ErrorInternalServerError)?,
            "TAQA" => get_taqa_stats(
                pool.get_ref(),
                &query.start_date,
                &query.end_date,
                has_financial_access,
            )
            .await
            .map_err(actix_web::error::ErrorInternalServerError)?,
            "Petromin" => get_petromin_stats(
                pool.get_ref(),
                &query.start_date,
                &query.end_date,
                has_financial_access,
            )
            .await
            .map_err(actix_web::error::ErrorInternalServerError)?,
            "Watanya" => get_watanya_stats(
                pool.get_ref(),
                &query.start_date,
                &query.end_date,
                has_financial_access,
            )
            .await
            .map_err(actix_web::error::ErrorInternalServerError)?,
            _ => vec![],
        };

        // Trip counts come from one pass over the whole filtered set, NOT from
        // summing the per-group figures below. A trip whose containers deliver
        // to two drop-off points appears in two groups, so summing counted it
        // twice -- reporting 1,468 for Jul-Aug 2026 against a true 1,307.
        let (total_trips, total_receipts) = get_trip_counts(
            pool.get_ref(),
            &company,
            &query.start_date,
            &query.end_date,
        )
        .await
        .map_err(actix_web::error::ErrorInternalServerError)?;
        let total_volume: f64 = details.iter().map(|d| d.total_volume).sum();
        let total_distance: f64 = details.iter().map(|d| d.total_distance).sum();

        let (total_revenue, total_car_rent, total_vat, total_amount) = if has_financial_access {
            let revenue: f64 = details.iter().map(|d| d.total_revenue).sum();
            let car_rent: f64 = details.iter().filter_map(|d| d.car_rental).sum();
            let vat: f64 = details.iter().filter_map(|d| d.vat).sum();
            let amount: f64 = details.iter().filter_map(|d| d.total_with_vat).sum();

            // For companies without car rental or vat (Petrol Arrows, Watanya), use revenue as total
            let final_car_rent = if car_rent == 0.0 {
                None
            } else {
                Some(car_rent)
            };
            let final_vat = if vat == 0.0 { None } else { Some(vat) };
            let final_amount = if amount == 0.0 {
                // If no total_with_vat in details, use revenue only
                Some(revenue)
            } else {
                Some(amount)
            };

            (revenue, final_car_rent, final_vat, final_amount)
        } else {
            // Users without financial access (permission < 3) get 0 for all financial data
            (0.0, None, None, None)
        };

        // Get route details
        let route_details = get_route_details(
            pool.get_ref(),
            &company,
            &query.start_date,
            &query.end_date,
            has_financial_access,
        )
        .await
        .map_err(actix_web::error::ErrorInternalServerError)?;

        statistics.push(TripStatistics {
            company: company.clone(),
            total_trips,
            total_receipts,
            total_volume,
            total_distance,
            total_revenue,
            total_car_rent,
            total_vat,
            total_amount,
            details: Some(details),
            route_details: Some(route_details),
        });
    }

    // Get stats by date
    let stats_by_date = get_stats_by_date(
        pool.get_ref(),
        &query.start_date,
        &query.end_date,
        query.company.as_deref(),
        has_financial_access,
    )
    .await
    .map_err(actix_web::error::ErrorInternalServerError)?;

    let car_totals = calculate_car_totals(&statistics);

    let response_data = TripStatisticsResponse {
        message: "Trip statistics retrieved successfully".to_string(),
        data: statistics,
        stats_by_date,
        has_financial_access,
        car_totals,
    };

    response(&response_data, use_msgpack).map_err(actix_web::error::ErrorInternalServerError)
}

pub async fn health_check() -> HttpResponse {
    HttpResponse::Ok().json(serde_json::json!({
        "status": "healthy",
        "service": "trip-stats-rust"
    }))
}

/* ------------------------------------------------------------------------ */
/* Per-route daily breakdown                                                 */
/* ------------------------------------------------------------------------ */

#[derive(serde::Deserialize)]
pub struct RouteDaysQuery {
    pub company: String,
    pub start_date: String,
    pub end_date: String,
    pub terminal: Option<String>,
    pub drop_off_point: Option<String>,
    pub fee: Option<f64>,
    pub route_name: Option<String>,
    /// `msgpack` for MessagePack, matching the statistics endpoint this panel
    /// sits inside.
    pub format: Option<String>,
}

/// `GET /api/v1/trip-statistics/route-days`
///
/// The day-by-day panel behind a route row. Permission 3, matching the
/// statistics endpoint it belongs to — it is the same aggregate at a finer
/// grain, not a per-trip disclosure.
///
/// Replaces a client-side grouping that pulled up to ten thousand raw trips
/// into the browser, truncated silently at that limit, and summed fee band
/// numbers as though they were money.
pub async fn get_route_days(
    pool: web::Data<PgPool>,
    query: web::Query<RouteDaysQuery>,
    req: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    let has_financial_access = req
        .extensions()
        .get::<Claims>()
        .and_then(|c| c.permission)
        .map(|p| p >= 3)
        .unwrap_or(false);

    let q = query.into_inner();
    let use_msgpack = q.format.as_deref() == Some("msgpack");
    let blank_is_none = |v: Option<String>| v.filter(|s| !s.trim().is_empty());

    let mut days = get_route_day_breakdown(
        pool.get_ref(),
        &q.company,
        &q.start_date,
        &q.end_date,
        blank_is_none(q.terminal).as_deref(),
        blank_is_none(q.drop_off_point).as_deref(),
        q.fee,
        blank_is_none(q.route_name).as_deref(),
    )
    .await
    .map_err(|e| {
        log::error!("route day breakdown failed: {e:#}");
        actix_web::error::ErrorInternalServerError("failed to fetch route days")
    })?;

    // Volume, distance and trip counts are operational and stay; the money is
    // zeroed for callers below the financial threshold, matching how the
    // statistics endpoint next door treats the same figures.
    if !has_financial_access {
        for day in &mut days {
            day.revenue = 0.0;
            day.revenue_total = 0.0;
        }
    }

    response(&serde_json::json!({ "data": days }), use_msgpack)
        .map_err(actix_web::error::ErrorInternalServerError)
}
