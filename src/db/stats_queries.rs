// db.rs - Revised with fixes for issues 5, 6, 7, 8 and extended Watanya fees

use sqlx::{PgPool, Row};
use anyhow::Result;
use std::collections::HashMap;

use crate::models::*;

pub async fn get_companies(
    pool: &PgPool,
    start_date: &str,
    end_date: &str,
    company_filter: Option<&str>,
) -> Result<Vec<String>> {
    let query = match (start_date.is_empty(), end_date.is_empty(), company_filter) {
        (false, false, Some(company)) => {
            sqlx::query_scalar(
                "SELECT DISTINCT company FROM trips 
                 WHERE deleted_at IS NULL 
                 AND date BETWEEN $1 AND $2 
                 AND company = $3
                 ORDER BY company"
            )
            .bind(start_date)
            .bind(end_date)
            .bind(company)
            .fetch_all(pool)
            .await?
        }
        (false, false, None) => {
            sqlx::query_scalar(
                "SELECT DISTINCT company FROM trips 
                 WHERE deleted_at IS NULL 
                 AND date BETWEEN $1 AND $2 
                 ORDER BY company"
            )
            .bind(start_date)
            .bind(end_date)
            .fetch_all(pool)
            .await?
        }
        (_, _, Some(company)) => {
            sqlx::query_scalar(
                "SELECT DISTINCT company FROM trips 
                 WHERE deleted_at IS NULL 
                 AND company = $1
                 ORDER BY company"
            )
            .bind(company)
            .fetch_all(pool)
            .await?
        }
        _ => {
            sqlx::query_scalar(
                "SELECT DISTINCT company FROM trips 
                 WHERE deleted_at IS NULL 
                 ORDER BY company"
            )
            .fetch_all(pool)
            .await?
        }
    };
    
    Ok(query)
}

pub async fn get_petrol_arrows_stats(
    pool: &PgPool,
    start_date: &str,
    end_date: &str,
    has_financial_access: bool,
) -> Result<Vec<TripStatisticsDetails>> {
    let query = r#"
        WITH trip_data AS (
            SELECT 
                t.drop_off_point,
                t.parent_trip_id,
                t.tank_capacity,
                COALESCE(fm.distance, 0.0) as distance,
                COALESCE(fm.fee::float8, 0.0) as fee,
                (t.tank_capacity * COALESCE(fm.fee::float8, 0.0) / 1000.0)::float8 as trip_revenue
            FROM trips t
            LEFT JOIN fee_mappings fm 
                ON t.company = fm.company 
                AND t.terminal = fm.terminal 
                AND t.drop_off_point = fm.drop_off_point
            WHERE t.company = 'Petrol Arrows'
                AND t.deleted_at IS NULL
                AND t.date BETWEEN $1 AND $2
        ),
        aggregates AS (
            SELECT 
                drop_off_point,
                COALESCE(COUNT(DISTINCT parent_trip_id) FILTER (WHERE parent_trip_id IS NOT NULL AND parent_trip_id != 0), 0) +
                COALESCE(COUNT(*) FILTER (WHERE parent_trip_id IS NULL OR parent_trip_id = 0), 0) as total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8 as total_volume,
                COALESCE(SUM(distance), 0.0)::float8 as total_distance,
                COALESCE(MAX(fee), 0.0)::float8 as fee,
                COALESCE(SUM(trip_revenue), 0.0)::float8 as total_revenue
            FROM trip_data
            GROUP BY drop_off_point
        )
        SELECT 
            drop_off_point as group_name,
            total_trips::bigint,
            total_volume,
            total_distance,
            fee,
            CASE WHEN $3 THEN total_revenue ELSE 0.0 END as total_revenue
        FROM aggregates
        ORDER BY drop_off_point
    "#;

    let rows = sqlx::query(query)
        .bind(start_date)
        .bind(end_date)
        .bind(has_financial_access)
        .fetch_all(pool)
        .await?;

    let details = rows
        .into_iter()
        .map(|row| TripStatisticsDetails {
            group_name: row.get("group_name"),
            total_trips: row.get("total_trips"),
            total_volume: row.get("total_volume"),
            total_distance: row.get("total_distance"),
            total_revenue: row.get("total_revenue"),
            fee: row.try_get("fee").ok(),
            car_rental: None,
            vat: None,
            total_with_vat: None,
            distinct_cars: None,
            distinct_days: None,
            car_days: None,
        })
        .collect();

    Ok(details)
}

pub async fn get_taqa_stats(
    pool: &PgPool,
    start_date: &str,
    end_date: &str,
    has_financial_access: bool,
) -> Result<Vec<TripStatisticsDetails>> {
    let query = r#"
        WITH trip_data AS (
            SELECT 
                t.terminal,
                t.parent_trip_id,
                t.car_no_plate,
                t.date,
                t.tank_capacity,
                COALESCE(fm.distance, 0.0) as distance,
                (CASE 
                    WHEN t.terminal IN ('Alex', 'Suez') THEN COALESCE(fm.distance, 0.0) * 45.5
                    ELSE 0.0
                END)::float8 as trip_revenue
            FROM trips t
            LEFT JOIN fee_mappings fm 
                ON t.company = fm.company 
                AND t.terminal = fm.terminal 
                AND t.drop_off_point = fm.drop_off_point
            WHERE t.company = 'TAQA'
                AND t.deleted_at IS NULL
                AND t.date BETWEEN $1 AND $2
        ),
        -- Calculate working days per car per month per terminal
        car_monthly_working_days AS (
            SELECT 
                terminal,
                car_no_plate,
                DATE_TRUNC('month', date::date) as month,
                COUNT(DISTINCT date)::int as working_days_in_month
            FROM trip_data
            GROUP BY terminal, car_no_plate, DATE_TRUNC('month', date::date)
        ),
        -- Calculate monthly rental per car per month (FIX #8: guard against division by zero)
        car_monthly_rentals AS (
            SELECT 
                terminal,
                car_no_plate,
                month,
                working_days_in_month,
                CASE 
                    WHEN working_days_in_month >= 28 THEN 43000.0
                    WHEN working_days_in_month > 0 THEN GREATEST(0.0, 43000.0 - ((28 - working_days_in_month) * 1535.71))
                    ELSE 0.0
                END::float8 as monthly_rental
            FROM car_monthly_working_days
        ),
        -- Sum rentals per car across all months
        car_total_rentals AS (
            SELECT 
                terminal,
                car_no_plate,
                SUM(monthly_rental)::float8 as total_car_rental,
                SUM(working_days_in_month)::bigint as total_working_days
            FROM car_monthly_rentals
            GROUP BY terminal, car_no_plate
        ),
        -- Aggregate by terminal
        car_rentals AS (
            SELECT 
                terminal,
                SUM(total_car_rental)::float8 as total_car_rental,
                SUM(total_working_days)::bigint as total_car_days
            FROM car_total_rentals
            GROUP BY terminal
        ),
        -- Aggregate trip data
        aggregates AS (
            SELECT 
                terminal,
                COALESCE(COUNT(DISTINCT parent_trip_id) FILTER (WHERE parent_trip_id IS NOT NULL AND parent_trip_id != 0), 0) +
                COALESCE(COUNT(*) FILTER (WHERE parent_trip_id IS NULL OR parent_trip_id = 0), 0) as total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8 as total_volume,
                COALESCE(SUM(distance), 0.0)::float8 as total_distance,
                COUNT(DISTINCT car_no_plate)::bigint as distinct_cars,
                COUNT(DISTINCT date)::bigint as distinct_days,
                COALESCE(SUM(trip_revenue), 0.0)::float8 as base_revenue
            FROM trip_data
            GROUP BY terminal
        )
        SELECT 
            a.terminal as group_name,
            a.total_trips::bigint,
            a.total_volume,
            a.total_distance,
            a.distinct_cars,
            a.distinct_days,
            COALESCE(cr.total_car_days, 0)::bigint as car_days,
            CASE WHEN $3 THEN a.base_revenue ELSE 0.0 END as base_revenue,
            CASE WHEN $3 THEN COALESCE(cr.total_car_rental, 0.0)::float8 ELSE NULL END as car_rental,
            CASE WHEN $3 THEN ((a.base_revenue + COALESCE(cr.total_car_rental, 0.0)) * 0.14)::float8 ELSE NULL END as vat,
            CASE 
                WHEN a.terminal IN ('Alex', 'Suez') THEN 45.5
                ELSE 0.0
            END as fee
        FROM aggregates a
        LEFT JOIN car_rentals cr ON a.terminal = cr.terminal
        ORDER BY a.terminal
    "#;

    let rows = sqlx::query(query)
        .bind(start_date)
        .bind(end_date)
        .bind(has_financial_access)
        .fetch_all(pool)
        .await?;

    let details = rows
        .into_iter()
        .map(|row| {
            let base_revenue: f64 = row.get("base_revenue");
            let car_rental: Option<f64> = row.try_get("car_rental").ok().flatten();
            let vat: Option<f64> = row.try_get("vat").ok().flatten();
            
            let total_with_vat = if has_financial_access {
                Some(
                    base_revenue + 
                    car_rental.unwrap_or(0.0) + 
                    vat.unwrap_or(0.0)
                )
            } else {
                None
            };

            TripStatisticsDetails {
                group_name: row.get("group_name"),
                total_trips: row.get("total_trips"),
                total_volume: row.get("total_volume"),
                total_distance: row.get("total_distance"),
                total_revenue: base_revenue,
                car_rental,
                vat,
                total_with_vat,
                fee: row.try_get("fee").ok(),
                distinct_cars: Some(row.get("distinct_cars")),
                distinct_days: Some(row.get("distinct_days")),
                car_days: Some(row.get("car_days")),
            }
        })
        .collect();

    Ok(details)
}

pub async fn get_petromin_stats(
    pool: &PgPool,
    start_date: &str,
    end_date: &str,
    has_financial_access: bool,
) -> Result<Vec<TripStatisticsDetails>> {
    let query = r#"
        WITH trip_data AS (
            SELECT 
                t.terminal,
                t.parent_trip_id,
                t.car_no_plate,
                t.date,
                t.tank_capacity,
                COALESCE(fm.distance, 0.0) as distance,
                (COALESCE(fm.distance, 0.0) * 42.5)::float8 as trip_revenue
            FROM trips t
            LEFT JOIN fee_mappings fm 
                ON t.company = fm.company 
                AND t.terminal = fm.terminal 
                AND t.drop_off_point = fm.drop_off_point
            WHERE t.company = 'Petromin'
                AND t.deleted_at IS NULL
                AND t.date BETWEEN $1 AND $2
        ),
        -- Calculate car-days (unique car-date combinations)
        car_days AS (
            SELECT 
                terminal,
                COUNT(DISTINCT car_no_plate || '-' || date)::bigint as total_car_days
            FROM trip_data
            GROUP BY terminal
        ),
        -- Aggregate trip data
        aggregates AS (
            SELECT 
                terminal,
                COALESCE(COUNT(DISTINCT parent_trip_id) FILTER (WHERE parent_trip_id IS NOT NULL AND parent_trip_id != 0), 0) +
                COALESCE(COUNT(*) FILTER (WHERE parent_trip_id IS NULL OR parent_trip_id = 0), 0) as total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8 as total_volume,
                COALESCE(SUM(distance), 0.0)::float8 as total_distance,
                COUNT(DISTINCT car_no_plate)::bigint as distinct_cars,
                COUNT(DISTINCT date)::bigint as distinct_days,
                COALESCE(SUM(trip_revenue), 0.0)::float8 as base_revenue
            FROM trip_data
            GROUP BY terminal
        )
        SELECT 
            a.terminal as group_name,
            a.total_trips::bigint,
            a.total_volume,
            a.total_distance,
            a.distinct_cars,
            a.distinct_days,
            COALESCE(cd.total_car_days, 0)::bigint as car_days,
            CASE WHEN $3 THEN a.base_revenue ELSE 0.0 END as base_revenue,
            CASE WHEN $3 THEN (COALESCE(cd.total_car_days, 0) * 2000.0)::float8 ELSE NULL END as car_rental,
            CASE WHEN $3 THEN ((a.base_revenue + COALESCE(cd.total_car_days, 0) * 2000.0) * 0.14)::float8 ELSE NULL END as vat,
            42.5 as fee
        FROM aggregates a
        LEFT JOIN car_days cd ON a.terminal = cd.terminal
        ORDER BY a.terminal
    "#;

    let rows = sqlx::query(query)
        .bind(start_date)
        .bind(end_date)
        .bind(has_financial_access)
        .fetch_all(pool)
        .await?;

    let details = rows
        .into_iter()
        .map(|row| {
            let base_revenue: f64 = row.get("base_revenue");
            let car_rental: Option<f64> = row.try_get("car_rental").ok().flatten();
            let vat: Option<f64> = row.try_get("vat").ok().flatten();
            
            let total_with_vat = if has_financial_access {
                Some(
                    base_revenue + 
                    car_rental.unwrap_or(0.0) + 
                    vat.unwrap_or(0.0)
                )
            } else {
                None
            };

            TripStatisticsDetails {
                group_name: row.get("group_name"),
                total_trips: row.get("total_trips"),
                total_volume: row.get("total_volume"),
                total_distance: row.get("total_distance"),
                total_revenue: base_revenue,
                car_rental,
                vat,
                total_with_vat,
                fee: row.try_get("fee").ok(),
                distinct_cars: Some(row.get("distinct_cars")),
                distinct_days: Some(row.get("distinct_days")),
                car_days: Some(row.get("car_days")),
            }
        })
        .collect();

    Ok(details)
}

// FIX #5: Watanya now includes unmapped trips (with 0 revenue)
pub async fn get_watanya_stats(
    pool: &PgPool,
    start_date: &str,
    end_date: &str,
    has_financial_access: bool,
) -> Result<Vec<TripStatisticsDetails>> {
    let query = r#"
        WITH trip_data AS (
            SELECT 
                t.parent_trip_id,
                t.tank_capacity,
                COALESCE(fm.distance, 0.0) as distance,
                COALESCE(fm.fee::float8, 0.0) as fee,
                -- Extended Watanya fees (1-15)
                (t.tank_capacity * 
                    CASE COALESCE(fm.fee::int, 0)
                        WHEN 1 THEN 95.0
                        WHEN 2 THEN 111.0
                        WHEN 3 THEN 118.0
                        WHEN 4 THEN 142.0
                        WHEN 5 THEN 167.0
                        WHEN 6 THEN 179.0
                        WHEN 7 THEN 191.0
                        WHEN 8 THEN 214.0
                        WHEN 9 THEN 238.0
                        WHEN 10 THEN 262.0
                        WHEN 11 THEN 286.0
                        WHEN 12 THEN 310.0
                        WHEN 13 THEN 334.0
                        WHEN 14 THEN 358.0
                        WHEN 15 THEN 382.0
                        ELSE 0.0
                    END / 1000.0)::float8 as trip_revenue
            FROM trips t
            LEFT JOIN fee_mappings fm 
                ON t.company = fm.company 
                AND t.terminal = fm.terminal 
                AND t.drop_off_point = fm.drop_off_point
            WHERE t.company = 'Watanya' 
                AND t.deleted_at IS NULL
                AND t.date BETWEEN $1 AND $2
                -- REMOVED: AND fm.fee IS NOT NULL (FIX #5)
        ),
        aggregates AS (
            SELECT 
                COALESCE(fee, 0) as fee,
                COALESCE(COUNT(DISTINCT parent_trip_id) FILTER (WHERE parent_trip_id IS NOT NULL AND parent_trip_id != 0), 0) +
                COALESCE(COUNT(*) FILTER (WHERE parent_trip_id IS NULL OR parent_trip_id = 0), 0) as total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8 as total_volume, 
                COALESCE(SUM(distance), 0.0)::float8 as total_distance,
                COALESCE(SUM(trip_revenue), 0.0)::float8 as base_revenue
            FROM trip_data
            GROUP BY fee
        )
        SELECT 
            CASE WHEN a.fee > 0 THEN 'Fee ' || a.fee::int::text ELSE 'Unmapped' END as group_name,
            a.total_trips::bigint,
            a.total_volume,
            a.total_distance,
            a.fee,
            CASE WHEN $3 THEN a.base_revenue ELSE 0.0 END as base_revenue,
            CASE WHEN $3 THEN (a.base_revenue * 0.14)::float8 ELSE NULL END as vat
        FROM aggregates a
        ORDER BY a.fee
    "#;

    let rows = sqlx::query(query)
        .bind(start_date)
        .bind(end_date)
        .bind(has_financial_access)
        .fetch_all(pool)
        .await?;

    let details = rows
        .into_iter()
        .map(|row| {
            let base_revenue: f64 = row.get("base_revenue");
            let vat: Option<f64> = row.try_get("vat").ok().flatten();
            
            let total_with_vat = if has_financial_access {
                Some(base_revenue + vat.unwrap_or(0.0))
            } else {
                None
            };

            TripStatisticsDetails {
                group_name: row.get("group_name"),
                total_trips: row.get("total_trips"),
                total_volume: row.get("total_volume"),
                total_distance: row.get("total_distance"),
                total_revenue: base_revenue,
                car_rental: None,
                vat,
                total_with_vat,
                fee: row.try_get("fee").ok(),
                distinct_cars: None,
                distinct_days: None,
                car_days: None,
            }
        })
        .collect();

    Ok(details)
}

pub async fn get_route_details(
    pool: &PgPool,
    company: &str,
    start_date: &str,
    end_date: &str,
    has_financial_access: bool,
) -> Result<Vec<RouteRevenueStats>> {
    match company {
        "Watanya" => get_watanya_route_details(pool, start_date, end_date, has_financial_access).await,
        "TAQA" => get_taqa_route_details(pool, start_date, end_date, has_financial_access).await,
        "Petromin" => get_petromin_route_details(pool, start_date, end_date, has_financial_access).await,
        "Petrol Arrows" => get_petrol_arrows_route_details(pool, start_date, end_date, has_financial_access).await,
        _ => Ok(vec![]),
    }
}

// FIX #5 & #6: Watanya route details - includes unmapped, fixed GROUP BY
async fn get_watanya_route_details(
    pool: &PgPool,
    start_date: &str,
    end_date: &str,
    has_financial_access: bool,
) -> Result<Vec<RouteRevenueStats>> {
    let query = r#"
        WITH trip_data AS (
            SELECT 
                t.car_no_plate,
                t.parent_trip_id,
                t.date,
                t.tank_capacity,
                COALESCE(fm.distance, 0.0) as distance,
                COALESCE(fm.fee::float8, 0.0) as fee,
                -- Extended Watanya fees
                (t.tank_capacity * 
                    CASE COALESCE(fm.fee::int, 0)
                        WHEN 1 THEN 95.0
                        WHEN 2 THEN 111.0
                        WHEN 3 THEN 118.0
                        WHEN 4 THEN 142.0
                        WHEN 5 THEN 167.0
                        WHEN 6 THEN 179.0
                        WHEN 7 THEN 191.0
                        WHEN 8 THEN 214.0
                        WHEN 9 THEN 238.0
                        WHEN 10 THEN 262.0
                        WHEN 11 THEN 286.0
                        WHEN 12 THEN 310.0
                        WHEN 13 THEN 334.0
                        WHEN 14 THEN 358.0
                        WHEN 15 THEN 382.0
                        ELSE 0.0
                    END / 1000.0)::float8 as trip_revenue
            FROM trips t
            LEFT JOIN fee_mappings fm 
                ON t.company = fm.company 
                AND t.terminal = fm.terminal 
                AND t.drop_off_point = fm.drop_off_point
            WHERE t.company = 'Watanya'
                AND t.deleted_at IS NULL
                AND t.date BETWEEN $1 AND $2
        ),
        -- FIX #6: Simplified aggregation without problematic GROUP BY
        car_stats AS (
            SELECT 
                COALESCE(fee, 0) as fee,
                car_no_plate,
                COALESCE(COUNT(DISTINCT parent_trip_id) FILTER (WHERE parent_trip_id IS NOT NULL AND parent_trip_id != 0), 0) +
                COALESCE(COUNT(*) FILTER (WHERE parent_trip_id IS NULL OR parent_trip_id = 0), 0) as total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8 as total_volume,
                COALESCE(SUM(distance), 0.0)::float8 as total_distance,
                COUNT(DISTINCT date)::bigint as working_days,
                COALESCE(SUM(trip_revenue), 0.0)::float8 as base_revenue
            FROM trip_data
            GROUP BY fee, car_no_plate
        )
        SELECT 
            fee,
            car_no_plate,
            total_trips::bigint,
            total_volume,
            total_distance,
            working_days,
            CASE WHEN $3 THEN base_revenue ELSE NULL END as total_revenue,
            CASE WHEN $3 THEN (base_revenue * 0.14)::float8 ELSE NULL END as vat,
            CASE WHEN $3 THEN (base_revenue * 1.14)::float8 ELSE NULL END as total_with_vat
        FROM car_stats
        ORDER BY fee, car_no_plate
    "#;

    let rows = sqlx::query(query)
        .bind(start_date)
        .bind(end_date)
        .bind(has_financial_access)
        .fetch_all(pool)
        .await?;

    let mut fee_stats: HashMap<i32, Vec<CarStats>> = HashMap::new();

    for row in rows {
        let fee: f64 = row.try_get("fee").unwrap_or(0.0);
        let fee_int = fee as i32;
        
        let car = CarStats {
            car_no_plate: row.get("car_no_plate"),
            total_trips: row.get("total_trips"),
            total_volume: row.get("total_volume"),
            total_distance: row.get("total_distance"),
            total_revenue: row.try_get("total_revenue").ok().flatten(),
            working_days: row.get("working_days"),
            car_rental: None,
            vat: row.try_get("vat").ok().flatten(),
            total_with_vat: row.try_get("total_with_vat").ok().flatten(),
        };

        fee_stats.entry(fee_int).or_insert_with(Vec::new).push(car);
    }

    let mut result = Vec::new();

    for (fee, cars) in fee_stats {
        let total_trips: i64 = cars.iter().map(|c| c.total_trips).sum();
        let total_volume: f64 = cars.iter().map(|c| c.total_volume).sum();
        let total_distance: f64 = cars.iter().map(|c| c.total_distance).sum();

        let (total_revenue, vat, total_with_vat) = if has_financial_access {
            (
                Some(cars.iter().filter_map(|c| c.total_revenue).sum()),
                Some(cars.iter().filter_map(|c| c.vat).sum()),
                Some(cars.iter().filter_map(|c| c.total_with_vat).sum()),
            )
        } else {
            (None, None, None)
        };

        let group_name = if fee > 0 {
            format!("Fee Category {}", fee)
        } else {
            "Unmapped".to_string()
        };

        result.push(RouteRevenueStats {
            route_name: group_name,
            total_trips,
            total_volume,
            total_distance,
            total_revenue,
            vat,
            car_rental: None,
            total_with_vat,
            fee: Some(fee as f64),
            route_type: "fee".to_string(),
            terminal: None,
            drop_off_point: None,
            fee_category: Some(fee),
            cars,
        });
    }

    result.sort_by_key(|r| r.fee_category);
    Ok(result)
}

// FIX #6 & #8: TAQA route details - fixed GROUP BY, guarded division
async fn get_taqa_route_details(
    pool: &PgPool,
    start_date: &str,
    end_date: &str,
    has_financial_access: bool,
) -> Result<Vec<RouteRevenueStats>> {
    let query = r#"
        WITH trip_data AS (
            SELECT 
                t.terminal,
                t.car_no_plate,
                t.parent_trip_id,
                t.date,
                t.tank_capacity,
                COALESCE(fm.distance, 0.0) as distance,
                (CASE 
                    WHEN t.terminal IN ('Alex', 'Suez') THEN COALESCE(fm.distance, 0.0) * 45.5
                    ELSE 0.0
                END)::float8 as trip_revenue
            FROM trips t
            LEFT JOIN fee_mappings fm 
                ON t.company = fm.company 
                AND t.terminal = fm.terminal 
                AND t.drop_off_point = fm.drop_off_point
            WHERE t.company = 'TAQA'
                AND t.deleted_at IS NULL
                AND t.date BETWEEN $1 AND $2
        ),
        -- Calculate working days per car per month per terminal
        car_monthly_working_days AS (
            SELECT 
                terminal,
                car_no_plate,
                DATE_TRUNC('month', date::date) as month,
                COUNT(DISTINCT date)::int as working_days_in_month
            FROM trip_data
            GROUP BY terminal, car_no_plate, DATE_TRUNC('month', date::date)
        ),
        -- Calculate monthly rental per car (FIX #8: guard against zero)
        car_monthly_rentals AS (
            SELECT 
                terminal,
                car_no_plate,
                month,
                working_days_in_month,
                CASE 
                    WHEN working_days_in_month >= 28 THEN 43000.0
                    WHEN working_days_in_month > 0 THEN GREATEST(0.0, 43000.0 - ((28 - working_days_in_month) * 1535.71))
                    ELSE 0.0
                END::float8 as monthly_rental
            FROM car_monthly_working_days
        ),
        -- Sum rentals per car across all months per terminal
        car_rental_totals AS (
            SELECT 
                terminal,
                car_no_plate,
                SUM(monthly_rental)::float8 as total_car_rental,
                SUM(working_days_in_month)::bigint as total_working_days
            FROM car_monthly_rentals
            GROUP BY terminal, car_no_plate
        ),
        -- FIX #6: Aggregate trip stats per car separately, then join rental
        car_trip_stats AS (
            SELECT 
                terminal,
                car_no_plate,
                COALESCE(COUNT(DISTINCT parent_trip_id) FILTER (WHERE parent_trip_id IS NOT NULL AND parent_trip_id != 0), 0) +
                COALESCE(COUNT(*) FILTER (WHERE parent_trip_id IS NULL OR parent_trip_id = 0), 0) as total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8 as total_volume,
                COALESCE(SUM(distance), 0.0)::float8 as total_distance,
                COALESCE(SUM(trip_revenue), 0.0)::float8 as base_revenue
            FROM trip_data
            GROUP BY terminal, car_no_plate
        ),
        -- Join trip stats with rental data
        car_stats AS (
            SELECT 
                cts.terminal,
                cts.car_no_plate,
                cts.total_trips,
                cts.total_volume,
                cts.total_distance,
                cts.base_revenue,
                COALESCE(crt.total_working_days, 0)::bigint as working_days,
                COALESCE(crt.total_car_rental, 0.0)::float8 as car_rental
            FROM car_trip_stats cts
            LEFT JOIN car_rental_totals crt 
                ON cts.terminal = crt.terminal 
                AND cts.car_no_plate = crt.car_no_plate
        )
        SELECT 
            terminal,
            car_no_plate,
            total_trips::bigint,
            total_volume,
            total_distance,
            working_days,
            CASE WHEN $3 THEN base_revenue ELSE NULL END as total_revenue,
            CASE WHEN $3 THEN car_rental ELSE NULL END as car_rental,
            CASE WHEN $3 THEN ((base_revenue + car_rental) * 0.14)::float8 ELSE NULL END as vat,
            CASE WHEN $3 THEN (base_revenue + car_rental + (base_revenue + car_rental) * 0.14)::float8 ELSE NULL END as total_with_vat
        FROM car_stats
        ORDER BY terminal, car_no_plate
    "#;

    let rows = sqlx::query(query)
        .bind(start_date)
        .bind(end_date)
        .bind(has_financial_access)
        .fetch_all(pool)
        .await?;

    let mut terminal_stats: HashMap<String, Vec<CarStats>> = HashMap::new();

    for row in rows {
        let terminal: String = row.get("terminal");
        
        let car = CarStats {
            car_no_plate: row.get("car_no_plate"),
            total_trips: row.get("total_trips"),
            total_volume: row.get("total_volume"),
            total_distance: row.get("total_distance"),
            total_revenue: row.try_get("total_revenue").ok().flatten(),
            working_days: row.get("working_days"),
            car_rental: row.try_get("car_rental").ok().flatten(),
            vat: row.try_get("vat").ok().flatten(),
            total_with_vat: row.try_get("total_with_vat").ok().flatten(),
        };

        terminal_stats.entry(terminal).or_insert_with(Vec::new).push(car);
    }

    let mut result = Vec::new();

    for (terminal, cars) in terminal_stats {
        let total_trips: i64 = cars.iter().map(|c| c.total_trips).sum();
        let total_volume: f64 = cars.iter().map(|c| c.total_volume).sum();
        let total_distance: f64 = cars.iter().map(|c| c.total_distance).sum();

        let (total_revenue, car_rental, vat, total_with_vat) = if has_financial_access {
            (
                Some(cars.iter().filter_map(|c| c.total_revenue).sum()),
                Some(cars.iter().filter_map(|c| c.car_rental).sum()),
                Some(cars.iter().filter_map(|c| c.vat).sum()),
                Some(cars.iter().filter_map(|c| c.total_with_vat).sum()),
            )
        } else {
            (None, None, None, None)
        };

        result.push(RouteRevenueStats {
            route_name: terminal.clone(),
            total_trips,
            total_volume,
            total_distance,
            total_revenue,
            vat,
            car_rental,
            total_with_vat,
            fee: Some(45.5),
            route_type: "terminal".to_string(),
            terminal: Some(terminal),
            drop_off_point: None,
            fee_category: None,
            cars,
        });
    }

    result.sort_by(|a, b| a.terminal.cmp(&b.terminal));
    Ok(result)
}

// FIX #6: Petromin route details - simplified GROUP BY
async fn get_petromin_route_details(
    pool: &PgPool,
    start_date: &str,
    end_date: &str,
    has_financial_access: bool,
) -> Result<Vec<RouteRevenueStats>> {
    let query = r#"
        WITH trip_data AS (
            SELECT 
                t.terminal,
                t.car_no_plate,
                t.parent_trip_id,
                t.date,
                t.tank_capacity,
                COALESCE(fm.distance, 0.0) as distance,
                (COALESCE(fm.distance, 0.0) * 42.5)::float8 as trip_revenue
            FROM trips t
            LEFT JOIN fee_mappings fm 
                ON t.company = fm.company 
                AND t.terminal = fm.terminal 
                AND t.drop_off_point = fm.drop_off_point
            WHERE t.company = 'Petromin'
                AND t.deleted_at IS NULL
                AND t.date BETWEEN $1 AND $2
        ),
        -- Calculate working days per car per terminal
        car_working_days AS (
            SELECT 
                terminal,
                car_no_plate,
                COUNT(DISTINCT date)::int as working_days
            FROM trip_data
            GROUP BY terminal, car_no_plate
        ),
        -- FIX #6: Separate trip aggregation
        car_trip_stats AS (
            SELECT 
                terminal,
                car_no_plate,
                COALESCE(COUNT(DISTINCT parent_trip_id) FILTER (WHERE parent_trip_id IS NOT NULL AND parent_trip_id != 0), 0) +
                COALESCE(COUNT(*) FILTER (WHERE parent_trip_id IS NULL OR parent_trip_id = 0), 0) as total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8 as total_volume,
                COALESCE(SUM(distance), 0.0)::float8 as total_distance,
                COALESCE(SUM(trip_revenue), 0.0)::float8 as base_revenue
            FROM trip_data
            GROUP BY terminal, car_no_plate
        ),
        -- Join trip stats with working days
        car_stats AS (
            SELECT 
                cts.terminal,
                cts.car_no_plate,
                cts.total_trips,
                cts.total_volume,
                cts.total_distance,
                cts.base_revenue,
                COALESCE(cwd.working_days, 0)::bigint as working_days,
                (COALESCE(cwd.working_days, 0) * 2000.0)::float8 as car_rental
            FROM car_trip_stats cts
            LEFT JOIN car_working_days cwd 
                ON cts.terminal = cwd.terminal 
                AND cts.car_no_plate = cwd.car_no_plate
        )
        SELECT 
            terminal,
            car_no_plate,
            total_trips::bigint,
            total_volume,
            total_distance,
            working_days,
            CASE WHEN $3 THEN base_revenue ELSE NULL END as total_revenue,
            CASE WHEN $3 THEN car_rental ELSE NULL END as car_rental,
            CASE WHEN $3 THEN ((base_revenue + car_rental) * 0.14)::float8 ELSE NULL END as vat,
            CASE WHEN $3 THEN (base_revenue + car_rental + (base_revenue + car_rental) * 0.14)::float8 ELSE NULL END as total_with_vat
        FROM car_stats
        ORDER BY terminal, car_no_plate
    "#;

    let rows = sqlx::query(query)
        .bind(start_date)
        .bind(end_date)
        .bind(has_financial_access)
        .fetch_all(pool)
        .await?;

    let mut terminal_stats: HashMap<String, Vec<CarStats>> = HashMap::new();

    for row in rows {
        let terminal: String = row.get("terminal");
        
        let car = CarStats {
            car_no_plate: row.get("car_no_plate"),
            total_trips: row.get("total_trips"),
            total_volume: row.get("total_volume"),
            total_distance: row.get("total_distance"),
            total_revenue: row.try_get("total_revenue").ok().flatten(),
            working_days: row.get("working_days"),
            car_rental: row.try_get("car_rental").ok().flatten(),
            vat: row.try_get("vat").ok().flatten(),
            total_with_vat: row.try_get("total_with_vat").ok().flatten(),
        };

        terminal_stats.entry(terminal).or_insert_with(Vec::new).push(car);
    }

    let mut result = Vec::new();

    for (terminal, cars) in terminal_stats {
        let total_trips: i64 = cars.iter().map(|c| c.total_trips).sum();
        let total_volume: f64 = cars.iter().map(|c| c.total_volume).sum();
        let total_distance: f64 = cars.iter().map(|c| c.total_distance).sum();

        let (total_revenue, car_rental, vat, total_with_vat) = if has_financial_access {
            (
                Some(cars.iter().filter_map(|c| c.total_revenue).sum()),
                Some(cars.iter().filter_map(|c| c.car_rental).sum()),
                Some(cars.iter().filter_map(|c| c.vat).sum()),
                Some(cars.iter().filter_map(|c| c.total_with_vat).sum()),
            )
        } else {
            (None, None, None, None)
        };

        result.push(RouteRevenueStats {
            route_name: terminal.clone(),
            total_trips,
            total_volume,
            total_distance,
            total_revenue,
            vat,
            car_rental,
            total_with_vat,
            fee: Some(42.5),
            route_type: "terminal".to_string(),
            terminal: Some(terminal),
            drop_off_point: None,
            fee_category: None,
            cars,
        });
    }

    result.sort_by(|a, b| a.terminal.cmp(&b.terminal));
    Ok(result)
}

// FIX #6: Petrol Arrows route details
async fn get_petrol_arrows_route_details(
    pool: &PgPool,
    start_date: &str,
    end_date: &str,
    has_financial_access: bool,
) -> Result<Vec<RouteRevenueStats>> {
    let query = r#"
        WITH trip_data AS (
            SELECT 
                t.terminal,
                t.drop_off_point,
                t.car_no_plate,
                t.parent_trip_id,
                t.date,
                t.tank_capacity,
                COALESCE(fm.distance, 0.0) as distance,
                COALESCE(fm.fee::float8, 0.0) as fee,
                (t.tank_capacity * COALESCE(fm.fee::float8, 0.0) / 1000.0)::float8 as trip_revenue
            FROM trips t
            LEFT JOIN fee_mappings fm 
                ON t.company = fm.company 
                AND t.terminal = fm.terminal 
                AND t.drop_off_point = fm.drop_off_point
            WHERE t.company = 'Petrol Arrows'
                AND t.deleted_at IS NULL
                AND t.date BETWEEN $1 AND $2
        ),
        car_stats AS (
            SELECT 
                terminal,
                drop_off_point,
                MAX(fee) as fee,
                car_no_plate,
                COALESCE(COUNT(DISTINCT parent_trip_id) FILTER (WHERE parent_trip_id IS NOT NULL AND parent_trip_id != 0), 0) +
                COALESCE(COUNT(*) FILTER (WHERE parent_trip_id IS NULL OR parent_trip_id = 0), 0) as total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8 as total_volume,
                COALESCE(SUM(distance), 0.0)::float8 as total_distance,
                COUNT(DISTINCT date)::bigint as working_days,
                COALESCE(SUM(trip_revenue), 0.0)::float8 as base_revenue
            FROM trip_data
            GROUP BY terminal, drop_off_point, car_no_plate
        )
        SELECT 
            terminal,
            drop_off_point,
            fee,
            car_no_plate,
            total_trips::bigint,
            total_volume,
            total_distance,
            working_days,
            CASE WHEN $3 THEN base_revenue ELSE NULL END as total_revenue,
            CASE WHEN $3 THEN base_revenue ELSE NULL END as total_with_vat
        FROM car_stats
        ORDER BY terminal, drop_off_point, car_no_plate
    "#;

    let rows = sqlx::query(query)
        .bind(start_date)
        .bind(end_date)
        .bind(has_financial_access)
        .fetch_all(pool)
        .await?;

    let mut route_stats: HashMap<(String, String), (f64, Vec<CarStats>)> = HashMap::new();

    for row in rows {
        let terminal: String = row.get("terminal");
        let drop_off_point: String = row.get("drop_off_point");
        let fee: f64 = row.try_get("fee").unwrap_or(0.0);
        let key = (terminal.clone(), drop_off_point.clone());
        
        let car = CarStats {
            car_no_plate: row.get("car_no_plate"),
            total_trips: row.get("total_trips"),
            total_volume: row.get("total_volume"),
            total_distance: row.get("total_distance"),
            total_revenue: row.try_get("total_revenue").ok().flatten(),
            working_days: row.get("working_days"),
            car_rental: None,
            vat: None,
            total_with_vat: row.try_get("total_with_vat").ok().flatten(),
        };

        route_stats.entry(key).or_insert_with(|| (fee, Vec::new())).1.push(car);
    }

    let mut result = Vec::new();

    for ((terminal, drop_off_point), (fee, cars)) in route_stats {
        let total_trips: i64 = cars.iter().map(|c| c.total_trips).sum();
        let total_volume: f64 = cars.iter().map(|c| c.total_volume).sum();
        let total_distance: f64 = cars.iter().map(|c| c.total_distance).sum();

        let (total_revenue, total_with_vat) = if has_financial_access {
            (
                Some(cars.iter().filter_map(|c| c.total_revenue).sum()),
                Some(cars.iter().filter_map(|c| c.total_with_vat).sum()),
            )
        } else {
            (None, None)
        };

        result.push(RouteRevenueStats {
            route_name: format!("{} to {}", terminal, drop_off_point),
            total_trips,
            total_volume,
            total_distance,
            total_revenue,
            vat: None,
            car_rental: None,
            total_with_vat,
            fee: Some(fee),
            route_type: "terminal-dropoff".to_string(),
            terminal: Some(terminal),
            drop_off_point: Some(drop_off_point),
            fee_category: None,
            cars,
        });
    }

    result.sort_by(|a, b| {
        a.terminal.cmp(&b.terminal)
            .then_with(|| a.drop_off_point.cmp(&b.drop_off_point))
    });
    Ok(result)
}

// FIX #7: Simplified stats by date - cleaner JOIN logic
pub async fn get_stats_by_date(
    pool: &PgPool,
    start_date: &str,
    end_date: &str,
    company_filter: Option<&str>,
    has_financial_access: bool,
) -> Result<Vec<TripRevenueDateResponse>> {
    let base_query = r#"
        WITH trip_data AS (
            SELECT 
                t.id,
                t.date,
                t.company,
                t.parent_trip_id,
                t.tank_capacity,
                t.car_no_plate,
                t.terminal,
                t.drop_off_point,
                COALESCE(fm.distance, 0.0) as distance,
                COALESCE(fm.fee::float8, 0.0) as fee,
                -- Calculate revenue per row based on company (extended Watanya fees)
                CASE 
                    WHEN t.company = 'Watanya' THEN
                        (t.tank_capacity * 
                            CASE COALESCE(fm.fee::int, 0)
                                WHEN 1 THEN 95.0
                                WHEN 2 THEN 111.0
                                WHEN 3 THEN 118.0
                                WHEN 4 THEN 142.0
                                WHEN 5 THEN 167.0
                                WHEN 6 THEN 179.0
                                WHEN 7 THEN 191.0
                                WHEN 8 THEN 214.0
                                WHEN 9 THEN 238.0
                                WHEN 10 THEN 262.0
                                WHEN 11 THEN 286.0
                                WHEN 12 THEN 310.0
                                WHEN 13 THEN 334.0
                                WHEN 14 THEN 358.0
                                WHEN 15 THEN 382.0
                                ELSE 0.0
                            END / 1000.0)::float8
                    WHEN t.company = 'TAQA' THEN
                        (CASE 
                            WHEN t.terminal IN ('Alex', 'Suez') THEN COALESCE(fm.distance, 0.0) * 45.5
                            ELSE 0.0
                        END)::float8
                    WHEN t.company = 'Petromin' THEN
                        (COALESCE(fm.distance, 0.0) * 42.5)::float8
                    WHEN t.company = 'Petrol Arrows' THEN
                        (t.tank_capacity * COALESCE(fm.fee::float8, 0.0) / 1000.0)::float8
                    ELSE 0.0
                END as trip_revenue
            FROM trips t
            LEFT JOIN fee_mappings fm 
                ON t.company = fm.company 
                AND t.terminal = fm.terminal 
                AND t.drop_off_point = fm.drop_off_point
            WHERE t.deleted_at IS NULL
                AND t.date BETWEEN $1 AND $2
    "#;

    let company_filter_clause = if company_filter.is_some() {
        "AND t.company = $3"
    } else {
        ""
    };

    let rest_of_query = r#"
        ),
        -- TAQA: Calculate working days per car per month
        taqa_car_monthly AS (
            SELECT 
                car_no_plate,
                DATE_TRUNC('month', date::date) as month,
                COUNT(DISTINCT date)::int as working_days_in_month
            FROM trip_data
            WHERE company = 'TAQA'
            GROUP BY car_no_plate, DATE_TRUNC('month', date::date)
        ),
        -- TAQA: Calculate monthly rental per car (FIX #8: guard zero)
        taqa_car_rental AS (
            SELECT 
                car_no_plate,
                month,
                working_days_in_month,
                CASE 
                    WHEN working_days_in_month >= 28 THEN 43000.0
                    WHEN working_days_in_month > 0 THEN GREATEST(0.0, 43000.0 - ((28 - working_days_in_month) * 1535.71))
                    ELSE 0.0
                END::float8 as monthly_rental
            FROM taqa_car_monthly
        ),
        -- TAQA: Allocate rental to each day (FIX #8: guard zero division)
        taqa_daily_allocation AS (
            SELECT 
                td.date,
                td.car_no_plate,
                CASE 
                    WHEN tcr.working_days_in_month > 0 
                    THEN tcr.monthly_rental / tcr.working_days_in_month 
                    ELSE 0.0 
                END as daily_share
            FROM (SELECT DISTINCT date, car_no_plate, DATE_TRUNC('month', date::date) as month FROM trip_data WHERE company = 'TAQA') td
            JOIN taqa_car_rental tcr ON td.car_no_plate = tcr.car_no_plate AND td.month = tcr.month
        ),
        -- TAQA: Sum daily rental by date
        taqa_daily_rental AS (
            SELECT 
                date,
                SUM(daily_share)::float8 as daily_car_rental
            FROM taqa_daily_allocation
            GROUP BY date
        ),
        -- Petromin: Car count per date
        petromin_daily_cars AS (
            SELECT 
                date,
                COUNT(DISTINCT car_no_plate)::int as car_count
            FROM trip_data
            WHERE company = 'Petromin'
            GROUP BY date
        ),
        -- FIX #7: Aggregate company stats without problematic JOINs in GROUP BY
        company_base_stats AS (
            SELECT 
                date,
                company,
                COALESCE(COUNT(DISTINCT parent_trip_id) FILTER (WHERE parent_trip_id IS NOT NULL AND parent_trip_id != 0), 0) +
                COALESCE(COUNT(*) FILTER (WHERE parent_trip_id IS NULL OR parent_trip_id = 0), 0) as total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8 as total_volume,
                COALESCE(SUM(distance), 0.0)::float8 as total_distance,
                COALESCE(SUM(trip_revenue), 0.0)::float8 as base_revenue
            FROM trip_data
            GROUP BY date, company
        ),
        -- FIX #7: Add car rental separately via LEFT JOIN
        company_with_rental AS (
            SELECT 
                cbs.date,
                cbs.company,
                cbs.total_trips,
                cbs.total_volume,
                cbs.total_distance,
                cbs.base_revenue,
                CASE 
                    WHEN cbs.company = 'TAQA' THEN COALESCE(tdr.daily_car_rental, 0.0)
                    WHEN cbs.company = 'Petromin' THEN COALESCE(pdc.car_count * 2000.0, 0.0)
                    ELSE 0.0
                END::float8 as car_rental
            FROM company_base_stats cbs
            LEFT JOIN taqa_daily_rental tdr ON cbs.date = tdr.date AND cbs.company = 'TAQA'
            LEFT JOIN petromin_daily_cars pdc ON cbs.date = pdc.date AND cbs.company = 'Petromin'
        ),
        -- Calculate VAT and totals
        company_final AS (
            SELECT 
                date,
                company,
                total_trips,
                total_volume,
                total_distance,
                base_revenue,
                car_rental,
                CASE 
                    WHEN company IN ('Watanya', 'TAQA', 'Petromin') THEN 
                        ((base_revenue + car_rental) * 0.14)::float8
                    ELSE 0.0
                END as vat,
                CASE 
                    WHEN company IN ('Watanya', 'TAQA', 'Petromin') THEN 
                        (base_revenue + car_rental + (base_revenue + car_rental) * 0.14)::float8
                    ELSE base_revenue
                END as total_revenue
            FROM company_with_rental
        ),
        -- Aggregate by date
        date_totals AS (
            SELECT 
                date,
                SUM(total_trips)::bigint as total_trips,
                SUM(total_volume)::float8 as total_volume,
                SUM(total_distance)::float8 as total_distance,
                SUM(total_revenue)::float8 as total_revenue
            FROM company_final
            GROUP BY date
        )
        SELECT 
            dt.date,
            dt.total_trips,
            dt.total_volume,
            dt.total_distance,
            dt.total_revenue,
            json_agg(
                json_build_object(
                    'company', cf.company,
                    'total_trips', cf.total_trips,
                    'total_volume', cf.total_volume,
                    'total_distance', cf.total_distance,
                    'total_revenue', cf.total_revenue,
                    'car_rental', CASE WHEN cf.car_rental > 0 THEN cf.car_rental ELSE NULL END,
                    'vat', CASE WHEN cf.vat > 0 THEN cf.vat ELSE NULL END
                )
                ORDER BY cf.company
            ) as company_details
        FROM date_totals dt
        JOIN company_final cf ON dt.date = cf.date
        GROUP BY dt.date, dt.total_trips, dt.total_volume, dt.total_distance, dt.total_revenue
        ORDER BY dt.date ASC
    "#;

    let full_query = format!("{}{}{}", base_query, company_filter_clause, rest_of_query);

    let rows = if let Some(company) = company_filter {
        sqlx::query(&full_query)
            .bind(start_date)
            .bind(end_date)
            .bind(company)
            .fetch_all(pool)
            .await?
    } else {
        sqlx::query(&full_query)
            .bind(start_date)
            .bind(end_date)
            .fetch_all(pool)
            .await?
    };

    let mut result = Vec::new();

    for row in rows {
        let date: String = row.get("date");
        let total_trips: i64 = row.get("total_trips");
        let total_volume: f64 = row.get("total_volume");
        let total_distance: f64 = row.get("total_distance");
        let total_revenue: f64 = row.get("total_revenue");
        
        let company_details_json: serde_json::Value = row.get("company_details");
        let mut company_details = Vec::new();
        
        if let Some(array) = company_details_json.as_array() {
            for item in array {
                if let (Some(company), Some(trips), Some(volume), Some(distance)) = (
                    item.get("company").and_then(|v| v.as_str()),
                    item.get("total_trips").and_then(|v| v.as_i64()),
                    item.get("total_volume").and_then(|v| v.as_f64()),
                    item.get("total_distance").and_then(|v| v.as_f64()),
                ) {
                    let revenue = item.get("total_revenue").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let car_rental = item.get("car_rental").and_then(|v| v.as_f64());
                    let vat = item.get("vat").and_then(|v| v.as_f64());
                    
                    company_details.push(CompanyRevenueDetails {
                        company: company.to_string(),
                        total_trips: trips,
                        total_volume: volume,
                        total_distance: distance,
                        total_revenue: if has_financial_access { Some(revenue) } else { None },
                        vat: if has_financial_access { vat } else { None },
                        car_rental: if has_financial_access { car_rental } else { None },
                        total_with_vat: if has_financial_access { Some(revenue) } else { None },
                    });
                }
            }
        }

        result.push(TripRevenueDateResponse {
            date,
            total_trips,
            total_volume,
            total_distance,
            total_revenue: if has_financial_access { Some(total_revenue) } else { None },
            company_details,
        });
    }

    Ok(result)
}

pub fn calculate_car_totals(statistics: &[TripStatistics]) -> Vec<CarTotal> {
    let mut car_totals_map: HashMap<String, CarTotal> = HashMap::new();

    for statistic in statistics {
        if let Some(route_details) = &statistic.route_details {
            for route_detail in route_details {
                for car in &route_detail.cars {
                    let car_total = car_totals_map
                        .entry(car.car_no_plate.clone())
                        .or_insert_with(|| CarTotal {
                            car_no_plate: car.car_no_plate.clone(),
                            liters: 0.0,
                            distance: 0.0,
                            base_revenue: 0.0,
                            vat: 0.0,
                            rent: 0.0,
                        });

                    car_total.liters += car.total_volume;
                    car_total.distance += car.total_distance;
                    car_total.base_revenue += car.total_revenue.unwrap_or(0.0);
                    car_total.vat += car.vat.unwrap_or(0.0);
                    car_total.rent += car.car_rental.unwrap_or(0.0);
                }
            }
        }
    }

    car_totals_map.into_values().collect()
}