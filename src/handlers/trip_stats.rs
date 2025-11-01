use actix_web::{web, HttpRequest, HttpResponse, HttpMessage};
use sqlx::PgPool;
use anyhow::Result;

use crate::auth::Claims;
use crate::models::*;
use crate::db::*;
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
            "Petrol Arrows" => {
                get_petrol_arrows_stats(
                    pool.get_ref(),
                    &query.start_date,
                    &query.end_date,
                    has_financial_access,
                )
                .await
                .map_err(actix_web::error::ErrorInternalServerError)?
            }
            "TAQA" => {
                get_taqa_stats(
                    pool.get_ref(),
                    &query.start_date,
                    &query.end_date,
                    has_financial_access,
                )
                .await
                .map_err(actix_web::error::ErrorInternalServerError)?
            }
            "Petromin" => {
                get_petromin_stats(
                    pool.get_ref(),
                    &query.start_date,
                    &query.end_date,
                    has_financial_access,
                )
                .await
                .map_err(actix_web::error::ErrorInternalServerError)?
            }
            "Watanya" => {
                get_watanya_stats(
                    pool.get_ref(),
                    &query.start_date,
                    &query.end_date,
                    has_financial_access,
                )
                .await
                .map_err(actix_web::error::ErrorInternalServerError)?
            }
            _ => vec![],
        };

        // Calculate company totals from details (aggregating child data)
        let total_trips: i64 = details.iter().map(|d| d.total_trips).sum();
        let total_volume: f64 = details.iter().map(|d| d.total_volume).sum();
        let total_distance: f64 = details.iter().map(|d| d.total_distance).sum();
        
        let (total_revenue, total_car_rent, total_vat, total_amount) = if has_financial_access {
            let revenue: f64 = details.iter()
                .map(|d| d.total_revenue)
                .sum();
            let car_rent: f64 = details.iter()
                .filter_map(|d| d.car_rental)
                .sum();
            let vat: f64 = details.iter()
                .filter_map(|d| d.vat)
                .sum();
            let amount: f64 = details.iter()
                .filter_map(|d| d.total_with_vat)
                .sum();
            
            // For companies without car rental or vat (Petrol Arrows, Watanya), use revenue as total
            let final_car_rent = if car_rent == 0.0 { None } else { Some(car_rent) };
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

    response(&response_data, use_msgpack)
        .map_err(actix_web::error::ErrorInternalServerError)
}

pub async fn health_check() -> HttpResponse {
    HttpResponse::Ok().json(serde_json::json!({
        "status": "healthy",
        "service": "trip-stats-rust"
    }))
}