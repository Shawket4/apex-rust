// db.rs - Regenerated with correct logic

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
                -- Calculate revenue per row
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
                -- Count distinct parent trips + standalone trips
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
                -- Calculate distance-based revenue per row
                (CASE 
                    WHEN t.terminal IN ('Alex', 'Suez') THEN COALESCE(fm.distance, 0.0) * 40.7
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
        -- Calculate working days per car for car rental
        car_working_days AS (
            SELECT 
                terminal,
                car_no_plate,
                COUNT(DISTINCT date)::int as working_days
            FROM trip_data
            WHERE parent_trip_id IS NULL OR parent_trip_id = 0
            GROUP BY terminal, car_no_plate
        ),
        -- Calculate car rental per terminal
        car_rentals AS (
            SELECT 
                terminal,
                SUM(
                    CASE 
                        WHEN working_days >= 28 THEN 43000.0
                        ELSE GREATEST(0.0, 43000.0 - ((28 - working_days) * 1433.0))
                    END
                )::float8 as total_car_rental,
                SUM(working_days)::bigint as total_car_days
            FROM car_working_days
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
                WHEN a.terminal IN ('Alex', 'Suez') THEN 40.7
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
                -- Calculate distance-based revenue per row
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
        -- Calculate car-days (unique car-date combinations for standalone trips)
        car_days AS (
            SELECT 
                terminal,
                COUNT(DISTINCT car_no_plate || '-' || date)::bigint as total_car_days
            FROM trip_data
            WHERE parent_trip_id IS NULL OR parent_trip_id = 0
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
            cd.total_car_days as car_days,
            CASE WHEN $3 THEN a.base_revenue ELSE 0.0 END as base_revenue,
            CASE WHEN $3 THEN (cd.total_car_days * 2000.0)::float8 ELSE NULL END as car_rental,
            CASE WHEN $3 THEN ((a.base_revenue + cd.total_car_days * 2000.0) * 0.14)::float8 ELSE NULL END as vat,
            42.5 as fee
        FROM aggregates a
        JOIN car_days cd ON a.terminal = cd.terminal
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
                fm.fee::float8 as fee,
                -- Calculate revenue per row based on fee tier
                (t.tank_capacity * 
                    CASE fm.fee::int
                        WHEN 1 THEN 82.5
                        WHEN 2 THEN 104.5
                        WHEN 3 THEN 126.5
                        WHEN 4 THEN 148.5
                        WHEN 5 THEN 170.5
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
                AND fm.fee IS NOT NULL
        ),
        aggregates AS (
            SELECT 
                fee,
                COALESCE(COUNT(DISTINCT parent_trip_id) FILTER (WHERE parent_trip_id IS NOT NULL AND parent_trip_id != 0), 0) +
                COALESCE(COUNT(*) FILTER (WHERE parent_trip_id IS NULL OR parent_trip_id = 0), 0) as total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8 as total_volume, 
                COALESCE(SUM(distance), 0.0)::float8 as total_distance,
                COALESCE(SUM(trip_revenue), 0.0)::float8 as base_revenue
            FROM trip_data
            GROUP BY fee
        )
        SELECT 
            'Fee ' || a.fee::text as group_name,
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
                fm.fee::float8 as fee,
                -- Calculate revenue per row
                (t.tank_capacity * 
                    CASE fm.fee::int
                        WHEN 1 THEN 82.5
                        WHEN 2 THEN 104.5
                        WHEN 3 THEN 126.5
                        WHEN 4 THEN 148.5
                        WHEN 5 THEN 170.5
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
                AND fm.fee IS NOT NULL
        ),
        car_stats AS (
            SELECT 
                fee,
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
            CASE WHEN $3 THEN (base_revenue + base_revenue * 0.14)::float8 ELSE NULL END as total_with_vat
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

        result.push(RouteRevenueStats {
            route_name: format!("Fee Category {}", fee),
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
                -- Calculate distance-based revenue per row
                (CASE 
                    WHEN t.terminal IN ('Alex', 'Suez') THEN COALESCE(fm.distance, 0.0) * 40.7
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
        -- Calculate working days per car per terminal
        car_working_days AS (
            SELECT 
                terminal,
                car_no_plate,
                COUNT(DISTINCT date)::int as working_days
            FROM trip_data
            WHERE parent_trip_id IS NULL OR parent_trip_id = 0
            GROUP BY terminal, car_no_plate
        ),
        car_stats AS (
            SELECT 
                td.terminal,
                td.car_no_plate,
                COALESCE(COUNT(DISTINCT td.parent_trip_id) FILTER (WHERE td.parent_trip_id IS NOT NULL AND td.parent_trip_id != 0), 0) +
                COALESCE(COUNT(*) FILTER (WHERE td.parent_trip_id IS NULL OR td.parent_trip_id = 0), 0) as total_trips,
                COALESCE(SUM(td.tank_capacity), 0.0)::float8 as total_volume,
                COALESCE(SUM(td.distance), 0.0)::float8 as total_distance,
                COALESCE(cwd.working_days, 0)::bigint as working_days,
                -- Calculate car rental based on working days
                CASE 
                    WHEN COALESCE(cwd.working_days, 0) >= 28 THEN 43000.0
                    ELSE GREATEST(0.0, 43000.0 - ((28 - COALESCE(cwd.working_days, 0)) * 1433.0))
                END::float8 as car_rental,
                COALESCE(SUM(td.trip_revenue), 0.0)::float8 as base_revenue
            FROM trip_data td
            LEFT JOIN car_working_days cwd 
                ON td.terminal = cwd.terminal 
                AND td.car_no_plate = cwd.car_no_plate
            GROUP BY td.terminal, td.car_no_plate, cwd.working_days
        )
        SELECT 
            terminal,
            car_no_plate,
            total_trips::bigint,
            total_volume,
            total_distance,
            0::bigint as working_days_display,  -- Display 0 for UI consistency
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
            working_days: row.get("working_days_display"),
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
            fee: Some(40.7),
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
                -- Calculate distance-based revenue per row
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
            WHERE parent_trip_id IS NULL OR parent_trip_id = 0
            GROUP BY terminal, car_no_plate
        ),
        car_stats AS (
            SELECT 
                td.terminal,
                td.car_no_plate,
                COALESCE(COUNT(DISTINCT td.parent_trip_id) FILTER (WHERE td.parent_trip_id IS NOT NULL AND td.parent_trip_id != 0), 0) +
                COALESCE(COUNT(*) FILTER (WHERE td.parent_trip_id IS NULL OR td.parent_trip_id = 0), 0) as total_trips,
                COALESCE(SUM(td.tank_capacity), 0.0)::float8 as total_volume,
                COALESCE(SUM(td.distance), 0.0)::float8 as total_distance,
                COALESCE(cwd.working_days, 0)::bigint as working_days,
                (COALESCE(cwd.working_days, 0) * 2000.0)::float8 as car_rental,
                COALESCE(SUM(td.trip_revenue), 0.0)::float8 as base_revenue
            FROM trip_data td
            LEFT JOIN car_working_days cwd 
                ON td.terminal = cwd.terminal 
                AND td.car_no_plate = cwd.car_no_plate
            GROUP BY td.terminal, td.car_no_plate, cwd.working_days
        )
        SELECT 
            terminal,
            car_no_plate,
            total_trips::bigint,
            total_volume,
            total_distance,
            0::bigint as working_days_display,  -- Display 0 for UI consistency
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
            working_days: row.get("working_days_display"),
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
                -- Calculate revenue per row
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
                fee,
                car_no_plate,
                COALESCE(COUNT(DISTINCT parent_trip_id) FILTER (WHERE parent_trip_id IS NOT NULL AND parent_trip_id != 0), 0) +
                COALESCE(COUNT(*) FILTER (WHERE parent_trip_id IS NULL OR parent_trip_id = 0), 0) as total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8 as total_volume,
                COALESCE(SUM(distance), 0.0)::float8 as total_distance,
                COUNT(DISTINCT date)::bigint as working_days,
                COALESCE(SUM(trip_revenue), 0.0)::float8 as base_revenue
            FROM trip_data
            GROUP BY terminal, drop_off_point, fee, car_no_plate
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

pub async fn get_stats_by_date(
    pool: &PgPool,
    start_date: &str,
    end_date: &str,
    company_filter: Option<&str>,
    has_financial_access: bool,
) -> Result<Vec<TripRevenueDateResponse>> {
    let (query, bindings) = if let Some(company) = company_filter {
        (
            r#"
            WITH trip_data AS (
                SELECT 
                    t.date,
                    t.company,
                    t.parent_trip_id,
                    t.tank_capacity,
                    t.car_no_plate,
                    t.terminal,
                    t.drop_off_point,
                    COALESCE(fm.distance, 0.0) as distance,
                    COALESCE(fm.fee::float8, 0.0) as fee,
                    -- Calculate revenue per row based on company
                    CASE 
                        WHEN t.company = 'Watanya' THEN
                            (t.tank_capacity * 
                                CASE fm.fee::int
                                    WHEN 1 THEN 82.5
                                    WHEN 2 THEN 104.5
                                    WHEN 3 THEN 126.5
                                    WHEN 4 THEN 148.5
                                    WHEN 5 THEN 170.5
                                    ELSE 0.0
                                END / 1000.0)::float8
                        WHEN t.company = 'TAQA' THEN
                            (CASE 
                                WHEN t.terminal IN ('Alex', 'Suez') THEN COALESCE(fm.distance, 0.0) * 40.7
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
                    AND t.company = $3
                    AND t.date BETWEEN $1 AND $2
            ),
            company_stats AS (
                SELECT 
                    date,
                    company,
                    COALESCE(COUNT(DISTINCT parent_trip_id) FILTER (WHERE parent_trip_id IS NOT NULL AND parent_trip_id != 0), 0) +
                    COALESCE(COUNT(*) FILTER (WHERE parent_trip_id IS NULL OR parent_trip_id = 0), 0) as total_trips,
                    COALESCE(SUM(tank_capacity), 0.0)::float8 as total_volume,
                    COALESCE(SUM(distance), 0.0)::float8 as total_distance,
                    COALESCE(SUM(trip_revenue), 0.0)::float8 as total_revenue
                FROM trip_data
                GROUP BY date, company
            ),
            date_totals AS (
                SELECT 
                    date,
                    COALESCE(SUM(total_trips), 0)::bigint as total_trips,
                    COALESCE(SUM(total_volume), 0.0)::float8 as total_volume,
                    COALESCE(SUM(total_distance), 0.0)::float8 as total_distance,
                    COALESCE(SUM(total_revenue), 0.0)::float8 as total_revenue
                FROM company_stats
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
                        'company', cs.company,
                        'total_trips', cs.total_trips,
                        'total_volume', cs.total_volume,
                        'total_distance', cs.total_distance,
                        'total_revenue', cs.total_revenue
                    )
                    ORDER BY cs.company
                ) as company_details
            FROM date_totals dt
            JOIN company_stats cs ON dt.date = cs.date
            GROUP BY dt.date, dt.total_trips, dt.total_volume, dt.total_distance, dt.total_revenue
            ORDER BY dt.date ASC
            "#,
            vec![start_date, end_date, company]
        )
    } else {
        (
            r#"
            WITH trip_data AS (
                SELECT 
                    t.date,
                    t.company,
                    t.parent_trip_id,
                    t.tank_capacity,
                    t.car_no_plate,
                    t.terminal,
                    t.drop_off_point,
                    COALESCE(fm.distance, 0.0) as distance,
                    COALESCE(fm.fee::float8, 0.0) as fee,
                    -- Calculate revenue per row based on company
                    CASE 
                        WHEN t.company = 'Watanya' THEN
                            (t.tank_capacity * 
                                CASE fm.fee::int
                                    WHEN 1 THEN 82.5
                                    WHEN 2 THEN 104.5
                                    WHEN 3 THEN 126.5
                                    WHEN 4 THEN 148.5
                                    WHEN 5 THEN 170.5
                                    ELSE 0.0
                                END / 1000.0)::float8
                        WHEN t.company = 'TAQA' THEN
                            (CASE 
                                WHEN t.terminal IN ('Alex', 'Suez') THEN COALESCE(fm.distance, 0.0) * 40.7
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
            ),
            company_stats AS (
                SELECT 
                    date,
                    company,
                    COALESCE(COUNT(DISTINCT parent_trip_id) FILTER (WHERE parent_trip_id IS NOT NULL AND parent_trip_id != 0), 0) +
                    COALESCE(COUNT(*) FILTER (WHERE parent_trip_id IS NULL OR parent_trip_id = 0), 0) as total_trips,
                    COALESCE(SUM(tank_capacity), 0.0)::float8 as total_volume,
                    COALESCE(SUM(distance), 0.0)::float8 as total_distance,
                    COALESCE(SUM(trip_revenue), 0.0)::float8 as total_revenue
                FROM trip_data
                GROUP BY date, company
            ),
            date_totals AS (
                SELECT 
                    date,
                    COALESCE(SUM(total_trips), 0)::bigint as total_trips,
                    COALESCE(SUM(total_volume), 0.0)::float8 as total_volume,
                    COALESCE(SUM(total_distance), 0.0)::float8 as total_distance,
                    COALESCE(SUM(total_revenue), 0.0)::float8 as total_revenue
                FROM company_stats
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
                        'company', cs.company,
                        'total_trips', cs.total_trips,
                        'total_volume', cs.total_volume,
                        'total_distance', cs.total_distance,
                        'total_revenue', cs.total_revenue
                    )
                    ORDER BY cs.company
                ) as company_details
            FROM date_totals dt
            JOIN company_stats cs ON dt.date = cs.date
            GROUP BY dt.date, dt.total_trips, dt.total_volume, dt.total_distance, dt.total_revenue
            ORDER BY dt.date ASC
            "#,
            vec![start_date, end_date]
        )
    };

    let rows = if bindings.len() == 3 {
        sqlx::query(query)
            .bind(bindings[0])
            .bind(bindings[1])
            .bind(bindings[2])
            .fetch_all(pool)
            .await?
    } else {
        sqlx::query(query)
            .bind(bindings[0])
            .bind(bindings[1])
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
                    
                    company_details.push(CompanyRevenueDetails {
                        company: company.to_string(),
                        total_trips: trips,
                        total_volume: volume,
                        total_distance: distance,
                        total_revenue: if has_financial_access { Some(revenue) } else { None },
                        vat: None,
                        car_rental: None,
                        total_with_vat: None,
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