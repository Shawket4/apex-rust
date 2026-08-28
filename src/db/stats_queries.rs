// db.rs - Complete with deleted_at fix and extended Watanya fees

use anyhow::Result;
use sqlx::{PgPool, Row};
use std::collections::HashMap;

use crate::models::*;

/// Substitutes the shared revenue fragments into a query template.
///
/// Every rate, band table and rental rule in these queries used to be typed out
/// inline, once per query — which is exactly how FalconGo and this file ended up
/// tapering TAQA's rental by two different numbers. The placeholders below are
/// the only spelling of those rules now; `db::revenue` is the only definition.
///
/// Substitution is by name rather than `format!` because these templates are
/// full of SQL braces that `format!` would demand be doubled, and a missed
/// escape is a silent query corruption. A placeholder that no longer exists in
/// `revenue` simply survives into the SQL and fails loudly at the database,
/// which is the failure mode worth having.
pub(crate) fn render(sql: &str) -> String {
    use crate::db::revenue::*;
    sql.replace("{trip_count}", &logical_trip_count_sql("parent_trip_id"))
        .replace("{trip_distance}", &trip_distance_sql("t", "fm"))
        .replace("{wa_band_rate}", &watanya_band_rate_sql("fm.fee"))
        .replace(
            "{pa_fee_rate}",
            &format!("COALESCE(fm.fee::float8, 0.0) / {LITRES_PER_FEE_UNIT:?}"),
        )
        .replace(
            "{taqa_monthly_rental}",
            &taqa_monthly_rental_sql("working_days_in_month"),
        )
        .replace("{taqa_rate}", &format!("{TAQA_RATE_PER_KM:?}"))
        .replace("{petromin_rate}", &format!("{PETROMIN_RATE_PER_KM:?}"))
        .replace(
            "{petromin_rental_per_car_day}",
            &format!("{PETROMIN_RENTAL_PER_CAR_DAY:?}"),
        )
        .replace("{vat_rate}", &format!("{VAT_RATE:?}"))
}

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
                 ORDER BY company",
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
                 ORDER BY company",
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
                 ORDER BY company",
            )
            .bind(company)
            .fetch_all(pool)
            .await?
        }
        _ => {
            sqlx::query_scalar(
                "SELECT DISTINCT company FROM trips 
                 WHERE deleted_at IS NULL 
                 ORDER BY company",
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
    let query = render(r#"
        WITH trip_data AS (
            SELECT 
                t.drop_off_point,
                t.parent_trip_id,
                t.tank_capacity,
                COALESCE(fm.distance, 0.0) as distance,
                {trip_distance} as trip_distance,
                COALESCE(fm.fee::float8, 0.0) as fee,
                (t.tank_capacity * {pa_fee_rate})::float8 as trip_revenue
            FROM trips t
            LEFT JOIN fee_mappings fm 
                ON t.company = fm.company 
                AND t.terminal = fm.terminal 
                AND t.drop_off_point = fm.drop_off_point
                AND fm.deleted_at IS NULL
            WHERE t.company = 'Petrol Arrows'
                AND t.deleted_at IS NULL
                AND t.date BETWEEN $1 AND $2
        ),
        aggregates AS (
            SELECT 
                drop_off_point,
                {trip_count} as total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8 as total_volume,
                COALESCE(SUM(trip_distance), 0.0)::float8 as total_distance,
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
    "#);

    let rows = sqlx::query(&query)
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
    let query = render(r#"
        WITH trip_data AS (
            SELECT 
                t.terminal,
                t.parent_trip_id,
                t.car_no_plate,
                t.date,
                t.tank_capacity,
                COALESCE(fm.distance, 0.0) as distance,
                {trip_distance} as trip_distance,
                ({trip_distance} * {taqa_rate})::float8 as trip_revenue
            FROM trips t
            LEFT JOIN fee_mappings fm 
                ON t.company = fm.company 
                AND t.terminal = fm.terminal 
                AND t.drop_off_point = fm.drop_off_point
                AND fm.deleted_at IS NULL
            WHERE t.company = 'TAQA'
                AND t.deleted_at IS NULL
                AND t.date BETWEEN $1 AND $2
        ),
        car_monthly_working_days AS (
            -- Days the car actually worked that month, across every terminal.
            -- A car cannot work two days in one day, so a date it served two
            -- terminals on counts ONCE. Grouping this by terminal, as it used
            -- to, credited such a day to each terminal and billed the fleet a
            -- day it had not earned.
            SELECT 
                car_no_plate,
                DATE_TRUNC('month', date::date) as month,
                COUNT(DISTINCT date)::int as working_days_in_month
            FROM trip_data
            GROUP BY car_no_plate, DATE_TRUNC('month', date::date)
        ),
        car_monthly_terminal_days AS (
            -- The same month split by terminal. Used only to attribute the one
            -- rental to the terminals that earned it, never to size it.
            SELECT 
                terminal,
                car_no_plate,
                DATE_TRUNC('month', date::date) as month,
                COUNT(DISTINCT date)::int as terminal_days
            FROM trip_data
            GROUP BY terminal, car_no_plate, DATE_TRUNC('month', date::date)
        ),
        car_monthly_rentals AS (
            -- One rental per car-month, divided between terminals in proportion
            -- to the days each saw. The shares sum back to that single rental
            -- exactly, so per-terminal reporting survives without inventing a
            -- second rental. Column shape is unchanged for everything below.
            SELECT 
                td.terminal,
                td.car_no_plate,
                td.month,
                td.terminal_days as working_days_in_month,
                (m.monthly_rental * td.terminal_days::float8
                   / SUM(td.terminal_days) OVER (PARTITION BY td.car_no_plate, td.month)
                )::float8 as monthly_rental
            FROM car_monthly_terminal_days td
            JOIN (
                SELECT 
                    car_no_plate,
                    month,
                    {taqa_monthly_rental} as monthly_rental
                FROM car_monthly_working_days
            ) m ON m.car_no_plate = td.car_no_plate AND m.month = td.month
        ),
        car_total_rentals AS (
            SELECT 
                terminal,
                car_no_plate,
                SUM(monthly_rental)::float8 as total_car_rental,
                SUM(working_days_in_month)::bigint as total_working_days
            FROM car_monthly_rentals
            GROUP BY terminal, car_no_plate
        ),
        car_rentals AS (
            SELECT 
                terminal,
                SUM(total_car_rental)::float8 as total_car_rental,
                SUM(total_working_days)::bigint as total_car_days
            FROM car_total_rentals
            GROUP BY terminal
        ),
        aggregates AS (
            SELECT 
                terminal,
                {trip_count} as total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8 as total_volume,
                COALESCE(SUM(trip_distance), 0.0)::float8 as total_distance,
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
            CASE WHEN $3 THEN ((a.base_revenue + COALESCE(cr.total_car_rental, 0.0)) * {vat_rate})::float8 ELSE NULL END as vat,
            {taqa_rate} as fee
        FROM aggregates a
        LEFT JOIN car_rentals cr ON a.terminal = cr.terminal
        ORDER BY a.terminal
    "#);

    let rows = sqlx::query(&query)
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
                Some(base_revenue + car_rental.unwrap_or(0.0) + vat.unwrap_or(0.0))
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
    let query = render(r#"
        WITH trip_data AS (
            SELECT 
                t.terminal,
                t.parent_trip_id,
                t.car_no_plate,
                t.date,
                t.tank_capacity,
                COALESCE(fm.distance, 0.0) as distance,
                {trip_distance} as trip_distance,
                ({trip_distance} * {petromin_rate})::float8 as trip_revenue
            FROM trips t
            LEFT JOIN fee_mappings fm 
                ON t.company = fm.company 
                AND t.terminal = fm.terminal 
                AND t.drop_off_point = fm.drop_off_point
                AND fm.deleted_at IS NULL
            WHERE t.company = 'Petromin'
                AND t.deleted_at IS NULL
                AND t.date BETWEEN $1 AND $2
        ),
        car_day_shares AS (
            -- Each (car, date) is ONE car-day. When a car served several
            -- terminals on the same date, that day is SPLIT between them rather
            -- than charged to each -- a car cannot be rented twice for one day.
            SELECT 
                terminal,
                car_no_plate,
                date,
                1.0::float8 / COUNT(*) OVER (PARTITION BY car_no_plate, date)
                    as day_share
            FROM (SELECT DISTINCT terminal, car_no_plate, date FROM trip_data) d
        ),
        car_days AS (
            SELECT 
                terminal,
                -- What this terminal is billed for: whole days it had the car
                -- to itself, fractions of days it shared.
                SUM(day_share)::float8 as chargeable_car_days,
                -- What it saw. Kept whole for display; summing this across
                -- terminals can exceed the fleet's real days, and should.
                COUNT(*)::bigint as total_car_days
            FROM car_day_shares
            GROUP BY terminal
        ),
        aggregates AS (
            SELECT 
                terminal,
                {trip_count} as total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8 as total_volume,
                COALESCE(SUM(trip_distance), 0.0)::float8 as total_distance,
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
            CASE WHEN $3 THEN (COALESCE(cd.chargeable_car_days, 0.0) * {petromin_rental_per_car_day})::float8 ELSE NULL END as car_rental,
            CASE WHEN $3 THEN ((a.base_revenue + COALESCE(cd.chargeable_car_days, 0.0) * {petromin_rental_per_car_day}) * {vat_rate})::float8 ELSE NULL END as vat,
            {petromin_rate} as fee
        FROM aggregates a
        LEFT JOIN car_days cd ON a.terminal = cd.terminal
        ORDER BY a.terminal
    "#);

    let rows = sqlx::query(&query)
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
                Some(base_revenue + car_rental.unwrap_or(0.0) + vat.unwrap_or(0.0))
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
    let query = render(r#"
        WITH trip_data AS (
            SELECT 
                t.parent_trip_id,
                t.tank_capacity,
                COALESCE(fm.distance, 0.0) as distance,
                {trip_distance} as trip_distance,
                COALESCE(fm.fee::float8, 0.0) as fee,
                (t.tank_capacity * 
                    {wa_band_rate} / 1000.0)::float8 as trip_revenue
            FROM trips t
            LEFT JOIN fee_mappings fm 
                ON t.company = fm.company 
                AND t.terminal = fm.terminal 
                AND t.drop_off_point = fm.drop_off_point
                AND fm.deleted_at IS NULL
            WHERE t.company = 'Watanya' 
                AND t.deleted_at IS NULL
                AND t.date BETWEEN $1 AND $2
        ),
        aggregates AS (
            SELECT 
                COALESCE(fee, 0) as fee,
                {trip_count} as total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8 as total_volume, 
                COALESCE(SUM(trip_distance), 0.0)::float8 as total_distance,
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
            CASE WHEN $3 THEN (a.base_revenue * {vat_rate})::float8 ELSE NULL END as vat
        FROM aggregates a
        ORDER BY a.fee
    "#);

    let rows = sqlx::query(&query)
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
        "Watanya" => {
            get_watanya_route_details(pool, start_date, end_date, has_financial_access).await
        }
        "TAQA" => get_taqa_route_details(pool, start_date, end_date, has_financial_access).await,
        "Petromin" => {
            get_petromin_route_details(pool, start_date, end_date, has_financial_access).await
        }
        "Petrol Arrows" => {
            get_petrol_arrows_route_details(pool, start_date, end_date, has_financial_access).await
        }
        _ => Ok(vec![]),
    }
}

async fn get_watanya_route_details(
    pool: &PgPool,
    start_date: &str,
    end_date: &str,
    has_financial_access: bool,
) -> Result<Vec<RouteRevenueStats>> {
    let query = render(r#"
        WITH trip_data AS (
            SELECT 
                t.car_no_plate,
                t.parent_trip_id,
                t.date,
                t.tank_capacity,
                COALESCE(fm.distance, 0.0) as distance,
                {trip_distance} as trip_distance,
                COALESCE(fm.fee::float8, 0.0) as fee,
                (t.tank_capacity * 
                    {wa_band_rate} / 1000.0)::float8 as trip_revenue
            FROM trips t
            LEFT JOIN fee_mappings fm 
                ON t.company = fm.company 
                AND t.terminal = fm.terminal 
                AND t.drop_off_point = fm.drop_off_point
                AND fm.deleted_at IS NULL
            WHERE t.company = 'Watanya'
                AND t.deleted_at IS NULL
                AND t.date BETWEEN $1 AND $2
        ),
        car_stats AS (
            SELECT 
                COALESCE(fee, 0) as fee,
                car_no_plate,
                {trip_count} as total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8 as total_volume,
                COALESCE(SUM(trip_distance), 0.0)::float8 as total_distance,
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
            CASE WHEN $3 THEN (base_revenue * {vat_rate})::float8 ELSE NULL END as vat,
            CASE WHEN $3 THEN (base_revenue * 1.14)::float8 ELSE NULL END as total_with_vat
        FROM car_stats
        ORDER BY fee, car_no_plate
    "#);

    let rows = sqlx::query(&query)
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

async fn get_taqa_route_details(
    pool: &PgPool,
    start_date: &str,
    end_date: &str,
    has_financial_access: bool,
) -> Result<Vec<RouteRevenueStats>> {
    let query = render(r#"
        WITH trip_data AS (
            SELECT 
                t.terminal,
                t.car_no_plate,
                t.parent_trip_id,
                t.date,
                t.tank_capacity,
                COALESCE(fm.distance, 0.0) as distance,
                {trip_distance} as trip_distance,
                ({trip_distance} * {taqa_rate})::float8 as trip_revenue
            FROM trips t
            LEFT JOIN fee_mappings fm 
                ON t.company = fm.company 
                AND t.terminal = fm.terminal 
                AND t.drop_off_point = fm.drop_off_point
                AND fm.deleted_at IS NULL
            WHERE t.company = 'TAQA'
                AND t.deleted_at IS NULL
                AND t.date BETWEEN $1 AND $2
        ),
        car_monthly_working_days AS (
            -- Days the car actually worked that month, across every terminal.
            -- A car cannot work two days in one day, so a date it served two
            -- terminals on counts ONCE. Grouping this by terminal, as it used
            -- to, credited such a day to each terminal and billed the fleet a
            -- day it had not earned.
            SELECT 
                car_no_plate,
                DATE_TRUNC('month', date::date) as month,
                COUNT(DISTINCT date)::int as working_days_in_month
            FROM trip_data
            GROUP BY car_no_plate, DATE_TRUNC('month', date::date)
        ),
        car_monthly_terminal_days AS (
            -- The same month split by terminal. Used only to attribute the one
            -- rental to the terminals that earned it, never to size it.
            SELECT 
                terminal,
                car_no_plate,
                DATE_TRUNC('month', date::date) as month,
                COUNT(DISTINCT date)::int as terminal_days
            FROM trip_data
            GROUP BY terminal, car_no_plate, DATE_TRUNC('month', date::date)
        ),
        car_monthly_rentals AS (
            -- One rental per car-month, divided between terminals in proportion
            -- to the days each saw. The shares sum back to that single rental
            -- exactly, so per-terminal reporting survives without inventing a
            -- second rental. Column shape is unchanged for everything below.
            SELECT 
                td.terminal,
                td.car_no_plate,
                td.month,
                td.terminal_days as working_days_in_month,
                (m.monthly_rental * td.terminal_days::float8
                   / SUM(td.terminal_days) OVER (PARTITION BY td.car_no_plate, td.month)
                )::float8 as monthly_rental
            FROM car_monthly_terminal_days td
            JOIN (
                SELECT 
                    car_no_plate,
                    month,
                    {taqa_monthly_rental} as monthly_rental
                FROM car_monthly_working_days
            ) m ON m.car_no_plate = td.car_no_plate AND m.month = td.month
        ),
        car_rental_totals AS (
            SELECT 
                terminal,
                car_no_plate,
                SUM(monthly_rental)::float8 as total_car_rental,
                SUM(working_days_in_month)::bigint as total_working_days
            FROM car_monthly_rentals
            GROUP BY terminal, car_no_plate
        ),
        car_trip_stats AS (
            SELECT 
                terminal,
                car_no_plate,
                {trip_count} as total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8 as total_volume,
                COALESCE(SUM(trip_distance), 0.0)::float8 as total_distance,
                COALESCE(SUM(trip_revenue), 0.0)::float8 as base_revenue
            FROM trip_data
            GROUP BY terminal, car_no_plate
        ),
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
            CASE WHEN $3 THEN ((base_revenue + car_rental) * {vat_rate})::float8 ELSE NULL END as vat,
            CASE WHEN $3 THEN (base_revenue + car_rental + (base_revenue + car_rental) * {vat_rate})::float8 ELSE NULL END as total_with_vat
        FROM car_stats
        ORDER BY terminal, car_no_plate
    "#);

    let rows = sqlx::query(&query)
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

        terminal_stats
            .entry(terminal)
            .or_insert_with(Vec::new)
            .push(car);
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
            fee: Some(crate::db::revenue::TAQA_RATE_PER_KM),
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
    let query = render(r#"
        WITH trip_data AS (
            SELECT 
                t.terminal,
                t.car_no_plate,
                t.parent_trip_id,
                t.date,
                t.tank_capacity,
                COALESCE(fm.distance, 0.0) as distance,
                {trip_distance} as trip_distance,
                ({trip_distance} * {petromin_rate})::float8 as trip_revenue
            FROM trips t
            LEFT JOIN fee_mappings fm 
                ON t.company = fm.company 
                AND t.terminal = fm.terminal 
                AND t.drop_off_point = fm.drop_off_point
                AND fm.deleted_at IS NULL
            WHERE t.company = 'Petromin'
                AND t.deleted_at IS NULL
                AND t.date BETWEEN $1 AND $2
        ),
        car_working_days AS (
            -- A date the car served two terminals is one day of rental, split
            -- between them, not a day charged to each.
            SELECT 
                terminal,
                car_no_plate,
                COUNT(*)::int as working_days,
                SUM(day_share)::float8 as chargeable_days
            FROM (
                SELECT 
                    terminal,
                    car_no_plate,
                    1.0::float8 / COUNT(*) OVER (PARTITION BY car_no_plate, date)
                        as day_share
                FROM (SELECT DISTINCT terminal, car_no_plate, date FROM trip_data) d
            ) shares
            GROUP BY terminal, car_no_plate
        ),
        car_trip_stats AS (
            SELECT 
                terminal,
                car_no_plate,
                {trip_count} as total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8 as total_volume,
                COALESCE(SUM(trip_distance), 0.0)::float8 as total_distance,
                COALESCE(SUM(trip_revenue), 0.0)::float8 as base_revenue
            FROM trip_data
            GROUP BY terminal, car_no_plate
        ),
        car_stats AS (
            SELECT 
                cts.terminal,
                cts.car_no_plate,
                cts.total_trips,
                cts.total_volume,
                cts.total_distance,
                cts.base_revenue,
                COALESCE(cwd.working_days, 0)::bigint as working_days,
                (COALESCE(cwd.chargeable_days, 0.0) * {petromin_rental_per_car_day})::float8 as car_rental
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
            CASE WHEN $3 THEN ((base_revenue + car_rental) * {vat_rate})::float8 ELSE NULL END as vat,
            CASE WHEN $3 THEN (base_revenue + car_rental + (base_revenue + car_rental) * {vat_rate})::float8 ELSE NULL END as total_with_vat
        FROM car_stats
        ORDER BY terminal, car_no_plate
    "#);

    let rows = sqlx::query(&query)
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

        terminal_stats
            .entry(terminal)
            .or_insert_with(Vec::new)
            .push(car);
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
            fee: Some(crate::db::revenue::PETROMIN_RATE_PER_KM),
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
    let query = render(r#"
        WITH trip_data AS (
            SELECT 
                t.terminal,
                t.drop_off_point,
                t.car_no_plate,
                t.parent_trip_id,
                t.date,
                t.tank_capacity,
                COALESCE(fm.distance, 0.0) as distance,
                {trip_distance} as trip_distance,
                COALESCE(fm.fee::float8, 0.0) as fee,
                (t.tank_capacity * {pa_fee_rate})::float8 as trip_revenue
            FROM trips t
            LEFT JOIN fee_mappings fm 
                ON t.company = fm.company 
                AND t.terminal = fm.terminal 
                AND t.drop_off_point = fm.drop_off_point
                AND fm.deleted_at IS NULL
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
                {trip_count} as total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8 as total_volume,
                COALESCE(SUM(trip_distance), 0.0)::float8 as total_distance,
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
    "#);

    let rows = sqlx::query(&query)
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

        route_stats
            .entry(key)
            .or_insert_with(|| (fee, Vec::new()))
            .1
            .push(car);
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
        a.terminal
            .cmp(&b.terminal)
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
                {trip_distance} as trip_distance,
                COALESCE(fm.fee::float8, 0.0) as fee,
                CASE 
                    WHEN t.company = 'Watanya' THEN
                        (t.tank_capacity * 
                            {wa_band_rate} / 1000.0)::float8
                    WHEN t.company = 'TAQA' THEN
                        ({trip_distance} * {taqa_rate})::float8
                    WHEN t.company = 'Petromin' THEN
                        ({trip_distance} * {petromin_rate})::float8
                    WHEN t.company = 'Petrol Arrows' THEN
                        (t.tank_capacity * {pa_fee_rate})::float8
                    ELSE 0.0
                END as trip_revenue
            FROM trips t
            LEFT JOIN fee_mappings fm 
                ON t.company = fm.company 
                AND t.terminal = fm.terminal 
                AND t.drop_off_point = fm.drop_off_point
                AND fm.deleted_at IS NULL
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
        taqa_car_monthly AS (
            SELECT 
                car_no_plate,
                DATE_TRUNC('month', date::date) as month,
                COUNT(DISTINCT date)::int as working_days_in_month
            FROM trip_data
            WHERE company = 'TAQA'
            GROUP BY car_no_plate, DATE_TRUNC('month', date::date)
        ),
        taqa_car_rental AS (
            SELECT 
                car_no_plate,
                month,
                working_days_in_month,
                {taqa_monthly_rental} as monthly_rental
            FROM taqa_car_monthly
        ),
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
        taqa_daily_rental AS (
            SELECT 
                date,
                SUM(daily_share)::float8 as daily_car_rental
            FROM taqa_daily_allocation
            GROUP BY date
        ),
        petromin_daily_cars AS (
            SELECT 
                date,
                COUNT(DISTINCT car_no_plate)::int as car_count
            FROM trip_data
            WHERE company = 'Petromin'
            GROUP BY date
        ),
        company_base_stats AS (
            SELECT 
                date,
                company,
                {trip_count} as total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8 as total_volume,
                COALESCE(SUM(trip_distance), 0.0)::float8 as total_distance,
                COALESCE(SUM(trip_revenue), 0.0)::float8 as base_revenue
            FROM trip_data
            GROUP BY date, company
        ),
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
                    WHEN cbs.company = 'Petromin' THEN COALESCE(pdc.car_count * {petromin_rental_per_car_day}, 0.0)
                    ELSE 0.0
                END::float8 as car_rental
            FROM company_base_stats cbs
            LEFT JOIN taqa_daily_rental tdr ON cbs.date = tdr.date AND cbs.company = 'TAQA'
            LEFT JOIN petromin_daily_cars pdc ON cbs.date = pdc.date AND cbs.company = 'Petromin'
        ),
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
                        ((base_revenue + car_rental) * {vat_rate})::float8
                    ELSE 0.0
                END as vat,
                CASE 
                    WHEN company IN ('Watanya', 'TAQA', 'Petromin') THEN 
                        (base_revenue + car_rental + (base_revenue + car_rental) * {vat_rate})::float8
                    ELSE base_revenue
                END as total_revenue
            FROM company_with_rental
        ),
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

    let full_query = render(&format!(
        "{}{}{}",
        base_query, company_filter_clause, rest_of_query
    ));

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
                    let revenue = item
                        .get("total_revenue")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    let car_rental = item.get("car_rental").and_then(|v| v.as_f64());
                    let vat = item.get("vat").and_then(|v| v.as_f64());

                    company_details.push(CompanyRevenueDetails {
                        company: company.to_string(),
                        total_trips: trips,
                        total_volume: volume,
                        total_distance: distance,
                        total_revenue: if has_financial_access {
                            Some(revenue)
                        } else {
                            None
                        },
                        vat: if has_financial_access { vat } else { None },
                        car_rental: if has_financial_access {
                            car_rental
                        } else {
                            None
                        },
                        total_with_vat: if has_financial_access {
                            Some(revenue)
                        } else {
                            None
                        },
                    });
                }
            }
        }

        result.push(TripRevenueDateResponse {
            date,
            total_trips,
            total_volume,
            total_distance,
            total_revenue: if has_financial_access {
                Some(total_revenue)
            } else {
                None
            },
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

/// Count logical trips and receipts for a company over a date range.
///
/// Two numbers that are easy to conflate and are both wanted:
///
///   * `trips`    -- a multi-container trip is ONE trip, however many receipts
///                   it carries. Standalone rows count as themselves.
///   * `receipts` -- individual rows, one per physical receipt.
///
/// This must be a single pass over the whole filtered set. The company total was
/// previously built by summing the per-drop-off-point counts, which counts a
/// trip once per group its containers touch -- so a trip delivering to two
/// points was counted twice. Over Jul-Aug 2026 that reported 1,468 against a
/// true 1,307.
pub async fn get_trip_counts(
    pool: &PgPool,
    company: &str,
    start_date: &str,
    end_date: &str,
) -> Result<(i64, i64), sqlx::Error> {
    let row = sqlx::query(&render(
        r#"
        SELECT
            {trip_count}
                AS trips,
            COUNT(*) AS receipts
        FROM trips
        WHERE company = $1
          AND deleted_at IS NULL
          AND date BETWEEN $2 AND $3
        "#,
    ))
    .bind(company)
    .bind(start_date)
    .bind(end_date)
    .fetch_one(pool)
    .await?;

    Ok((row.get::<i64, _>("trips"), row.get::<i64, _>("receipts")))
}

/* ------------------------------------------------------------------------ */
/* Per-route daily breakdown                                                 */
/* ------------------------------------------------------------------------ */

/// One day of a single route's activity.
#[derive(Debug, serde::Serialize)]
pub struct RouteDayRow {
    pub date: String,
    /// Logical trips: a multi-container trip counts once.
    pub trips: i64,
    pub volume: f64,
    pub distance: f64,
    /// Base revenue — what the trips themselves earned, so the days sum to the
    /// route's revenue in the parent row.
    pub revenue: f64,
    /// Base plus this day's share of car rental and VAT. Sums to the company
    /// total in the statistics header.
    pub revenue_total: f64,
    pub car_count: i64,
}

/// The daily breakdown behind one route row on the statistics page.
///
/// This used to be done in the browser: the dashboard downloaded up to ten
/// thousand raw trips and grouped them in JavaScript. Two things were wrong
/// with that beyond the payload size. The row count silently truncated at the
/// limit, and the revenue it summed was `trip.revenue || trip.fee` — the trips
/// table's `revenue` column is not maintained, so it fell through to `fee`,
/// which for Watanya is a fee BAND NUMBER between 1 and 15 rather than money.
///
/// Route matching mirrors the precedence the client used: terminal plus
/// drop-off point if both are known, then terminal alone, then fee band, and
/// finally a name that may be either.
pub async fn get_route_day_breakdown(
    pool: &PgPool,
    company: &str,
    start_date: &str,
    end_date: &str,
    terminal: Option<&str>,
    drop_off_point: Option<&str>,
    fee: Option<f64>,
    route_name: Option<&str>,
) -> Result<Vec<RouteDayRow>> {
    use crate::db::revenue::allocation::per_row_revenue_cte;

    let cte = per_row_revenue_cte("t.company = $1 AND t.date BETWEEN $2 AND $3");
    let sql = render(&format!(
        r#"
        WITH {cte}
        SELECT
            r.date,
            {{trip_count}}::bigint                            AS trips,
            COALESCE(SUM(r.tank_capacity), 0)::float8         AS volume,
            COALESCE(SUM(r.trip_distance), 0.0)::float8      AS distance,
            COALESCE(SUM(r.base_revenue), 0.0)::float8        AS revenue,
            COALESCE(SUM(r.allocated_total), 0.0)::float8     AS revenue_total,
            COUNT(DISTINCT r.car_no_plate)::bigint            AS car_count
        FROM revenue r
        WHERE CASE
                WHEN $4::text IS NOT NULL AND $5::text IS NOT NULL
                    THEN r.terminal = $4 AND r.drop_off_point = $5
                WHEN $4::text IS NOT NULL THEN r.terminal = $4
                WHEN $6::float8 IS NOT NULL THEN r.fee_value = $6
                WHEN $7::text IS NOT NULL
                    THEN r.drop_off_point = $7 OR r.terminal = $7
                ELSE TRUE
              END
        GROUP BY r.date
        ORDER BY r.date DESC
        "#
    ));

    let rows = sqlx::query(&sql)
        .bind(company)
        .bind(start_date)
        .bind(end_date)
        .bind(terminal)
        .bind(drop_off_point)
        .bind(fee)
        .bind(route_name)
        .fetch_all(pool)
        .await?;

    Ok(rows
        .into_iter()
        .map(|r| RouteDayRow {
            date: r.get("date"),
            trips: r.get("trips"),
            volume: r.get("volume"),
            distance: r.get("distance"),
            revenue: r.get("revenue"),
            revenue_total: r.get("revenue_total"),
            car_count: r.get("car_count"),
        })
        .collect())
}
