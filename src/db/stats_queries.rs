//! The trip statistics page's queries.
//!
//! Every money figure here is a sum over the stored revenue FalconGo prices on
//! write — `trips.base_revenue` and the `trip_revenue` view's allocated rental
//! and VAT. Nothing is computed from a rate in this file; see
//! [`crate::db::revenue`] for where the numbers come from and how a car's
//! monthly rental is attributed to a date range.

use anyhow::Result;
use sqlx::{PgPool, Row};
use std::collections::HashMap;

use crate::db::revenue::allocation::per_row_revenue_cte;
use crate::db::revenue::{fee_visible, per_km_rate_on};
use crate::models::*;

/// Substitutes the shared trip-shape fragments into a query template.
///
/// Substitution is by name rather than `format!` because these templates are
/// full of SQL braces that `format!` would demand be doubled, and a missed
/// escape is a silent query corruption. A placeholder that no longer exists
/// simply survives into the SQL and fails loudly at the database, which is the
/// failure mode worth having.
pub(crate) fn render(sql: &str) -> String {
    use crate::db::trip_shape::*;
    sql.replace("{trip_count}", &logical_trip_count_sql("parent_trip_id"))
        .replace("{trip_distance}", &trip_distance_sql("t", "fm"))
}

/// The stored-revenue chain for one company over `$1..$2`. `company` is one of
/// the four literal names below, never caller input.
fn company_window(company: &'static str) -> String {
    per_row_revenue_cte(&format!(
        "t.company = '{company}' AND t.date BETWEEN $1 AND $2"
    ))
}

/// A nullable float8 column, `None` when NULL.
fn opt_f64(row: &sqlx::postgres::PgRow, name: &str) -> Option<f64> {
    row.try_get::<Option<f64>, _>(name).ok().flatten()
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
    let query = render(&format!(
        r#"
        WITH {cte},
        aggregates AS (
            SELECT
                drop_off_point,
                {{trip_count}} AS total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8 AS total_volume,
                COALESCE(SUM(trip_distance), 0.0)::float8 AS total_distance,
                COALESCE(MAX(fee_value), 0.0)::float8     AS fee,
                COALESCE(SUM(revenue_base), 0.0)::float8  AS total_revenue
            FROM revenue
            GROUP BY drop_off_point
        )
        SELECT
            drop_off_point AS group_name,
            total_trips::bigint,
            total_volume,
            total_distance,
            -- The fee is Petrol Arrows' price per 1,000 L: money.
            CASE WHEN $3 THEN fee END AS fee,
            CASE WHEN $3 THEN total_revenue ELSE 0.0 END AS total_revenue
        FROM aggregates
        ORDER BY drop_off_point
        "#,
        cte = company_window("Petrol Arrows"),
    ));

    let rows = sqlx::query(&query)
        .bind(start_date)
        .bind(end_date)
        .bind(has_financial_access)
        .fetch_all(pool)
        .await?;

    Ok(rows
        .into_iter()
        .map(|row| TripStatisticsDetails {
            group_name: row.get("group_name"),
            total_trips: row.get("total_trips"),
            total_volume: row.get("total_volume"),
            total_distance: row.get("total_distance"),
            total_revenue: row.get("total_revenue"),
            fee: opt_f64(&row, "fee"),
            car_rental: None,
            vat: None,
            total_with_vat: None,
            distinct_cars: None,
            distinct_days: None,
            car_days: None,
        })
        .collect())
}

/// TAQA and Petromin: per-km base revenue plus car rental, both per terminal.
///
/// Rental is the terminal's rows' allocated shares of their car-months' stored
/// rental, so it is exactly what the trips list shows against those rows. VAT
/// is the stored VAT on the base plus the rental's stored VAT share.
async fn rental_company_stats(
    pool: &PgPool,
    company: &'static str,
    start_date: &str,
    end_date: &str,
    has_financial_access: bool,
) -> Result<Vec<TripStatisticsDetails>> {
    let query = render(&format!(
        r#"
        WITH {cte},
        car_days AS (
            -- Days each terminal had a car, counted per (car, date): what the
            -- terminal saw, not what it was billed for. A date shared with
            -- another terminal counts for both, and should.
            SELECT terminal, COUNT(*)::bigint AS total_car_days
            FROM (SELECT DISTINCT terminal, car_no_plate, date FROM revenue) d
            GROUP BY terminal
        ),
        aggregates AS (
            SELECT
                terminal,
                {{trip_count}} AS total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8    AS total_volume,
                COALESCE(SUM(trip_distance), 0.0)::float8    AS total_distance,
                COUNT(DISTINCT car_no_plate)::bigint         AS distinct_cars,
                COUNT(DISTINCT date)::bigint                 AS distinct_days,
                COALESCE(SUM(revenue_base), 0.0)::float8     AS base_revenue,
                COALESCE(SUM(allocated_rental), 0.0)::float8 AS car_rental,
                COALESCE(SUM(allocated_vat), 0.0)::float8    AS vat
            FROM revenue
            GROUP BY terminal
        )
        SELECT
            a.terminal AS group_name,
            a.total_trips::bigint,
            a.total_volume,
            a.total_distance,
            a.distinct_cars,
            a.distinct_days,
            COALESCE(cd.total_car_days, 0)::bigint AS car_days,
            CASE WHEN $3 THEN a.base_revenue ELSE 0.0 END AS base_revenue,
            CASE WHEN $3 THEN a.car_rental END AS car_rental,
            CASE WHEN $3 THEN a.vat END AS vat
        FROM aggregates a
        LEFT JOIN car_days cd ON cd.terminal = a.terminal
        ORDER BY a.terminal
        "#,
        cte = company_window(company),
    ));

    let rows = sqlx::query(&query)
        .bind(start_date)
        .bind(end_date)
        .bind(has_financial_access)
        .fetch_all(pool)
        .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let base_revenue: f64 = row.get("base_revenue");
            let car_rental = opt_f64(&row, "car_rental");
            let vat = opt_f64(&row, "vat");
            let total_with_vat = has_financial_access
                .then(|| base_revenue + car_rental.unwrap_or(0.0) + vat.unwrap_or(0.0));

            TripStatisticsDetails {
                group_name: row.get("group_name"),
                total_trips: row.get("total_trips"),
                total_volume: row.get("total_volume"),
                total_distance: row.get("total_distance"),
                total_revenue: base_revenue,
                car_rental,
                vat,
                total_with_vat,
                // Never populated for the per-km companies (the old literal
                // rate decoded as NUMERIC and was always dropped); the rate is
                // on the route rows instead.
                fee: None,
                distinct_cars: Some(row.get("distinct_cars")),
                distinct_days: Some(row.get("distinct_days")),
                car_days: Some(row.get("car_days")),
            }
        })
        .collect())
}

pub async fn get_taqa_stats(
    pool: &PgPool,
    start_date: &str,
    end_date: &str,
    has_financial_access: bool,
) -> Result<Vec<TripStatisticsDetails>> {
    rental_company_stats(pool, "TAQA", start_date, end_date, has_financial_access).await
}

pub async fn get_petromin_stats(
    pool: &PgPool,
    start_date: &str,
    end_date: &str,
    has_financial_access: bool,
) -> Result<Vec<TripStatisticsDetails>> {
    rental_company_stats(pool, "Petromin", start_date, end_date, has_financial_access).await
}

pub async fn get_watanya_stats(
    pool: &PgPool,
    start_date: &str,
    end_date: &str,
    has_financial_access: bool,
) -> Result<Vec<TripStatisticsDetails>> {
    let query = render(&format!(
        r#"
        WITH {cte},
        aggregates AS (
            SELECT
                fee_value AS fee,
                {{trip_count}} AS total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8 AS total_volume,
                COALESCE(SUM(trip_distance), 0.0)::float8 AS total_distance,
                COALESCE(SUM(revenue_base), 0.0)::float8  AS base_revenue,
                COALESCE(SUM(allocated_vat), 0.0)::float8 AS vat
            FROM revenue
            GROUP BY fee_value
        )
        SELECT
            CASE WHEN a.fee > 0 THEN 'Fee ' || a.fee::int::text ELSE 'Unmapped' END AS group_name,
            a.total_trips::bigint,
            a.total_volume,
            a.total_distance,
            -- A band number, not money: the page keys a band's routes on it.
            a.fee,
            CASE WHEN $3 THEN a.base_revenue ELSE 0.0 END AS base_revenue,
            CASE WHEN $3 THEN a.vat END AS vat
        FROM aggregates a
        ORDER BY a.fee
        "#,
        cte = company_window("Watanya"),
    ));

    let rows = sqlx::query(&query)
        .bind(start_date)
        .bind(end_date)
        .bind(has_financial_access)
        .fetch_all(pool)
        .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let base_revenue: f64 = row.get("base_revenue");
            let vat = opt_f64(&row, "vat");
            TripStatisticsDetails {
                group_name: row.get("group_name"),
                total_trips: row.get("total_trips"),
                total_volume: row.get("total_volume"),
                total_distance: row.get("total_distance"),
                total_revenue: base_revenue,
                car_rental: None,
                vat,
                total_with_vat: has_financial_access.then(|| base_revenue + vat.unwrap_or(0.0)),
                fee: opt_f64(&row, "fee"),
                distinct_cars: None,
                distinct_days: None,
                car_days: None,
            }
        })
        .collect())
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
        "TAQA" => {
            rental_company_route_details(pool, "TAQA", start_date, end_date, has_financial_access)
                .await
        }
        "Petromin" => {
            rental_company_route_details(pool, "Petromin", start_date, end_date, has_financial_access)
                .await
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
    let query = render(&format!(
        r#"
        WITH {cte}
        SELECT
            fee_value AS fee,
            car_no_plate,
            ({{trip_count}})::bigint AS total_trips,
            COALESCE(SUM(tank_capacity), 0.0)::float8 AS total_volume,
            COALESCE(SUM(trip_distance), 0.0)::float8 AS total_distance,
            COUNT(DISTINCT date)::bigint              AS working_days,
            CASE WHEN $3 THEN COALESCE(SUM(revenue_base), 0.0)::float8 END AS total_revenue,
            CASE WHEN $3 THEN COALESCE(SUM(allocated_vat), 0.0)::float8 END AS vat,
            CASE WHEN $3 THEN COALESCE(SUM(allocated_total), 0.0)::float8 END AS total_with_vat
        FROM revenue
        GROUP BY fee_value, car_no_plate
        ORDER BY fee_value, car_no_plate
        "#,
        cte = company_window("Watanya"),
    ));

    let rows = sqlx::query(&query)
        .bind(start_date)
        .bind(end_date)
        .bind(has_financial_access)
        .fetch_all(pool)
        .await?;

    let mut fee_stats: HashMap<i32, Vec<CarStats>> = HashMap::new();
    for row in rows {
        let fee: f64 = row.try_get("fee").unwrap_or(0.0);
        let car = CarStats {
            car_no_plate: row.get("car_no_plate"),
            total_trips: row.get("total_trips"),
            total_volume: row.get("total_volume"),
            total_distance: row.get("total_distance"),
            total_revenue: opt_f64(&row, "total_revenue"),
            working_days: row.get("working_days"),
            car_rental: None,
            vat: opt_f64(&row, "vat"),
            total_with_vat: opt_f64(&row, "total_with_vat"),
        };
        fee_stats.entry(fee as i32).or_default().push(car);
    }

    let mut result = Vec::new();
    for (fee, cars) in fee_stats {
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
            route_name: if fee > 0 {
                format!("Fee Category {}", fee)
            } else {
                "Unmapped".to_string()
            },
            total_trips: cars.iter().map(|c| c.total_trips).sum(),
            total_volume: cars.iter().map(|c| c.total_volume).sum(),
            total_distance: cars.iter().map(|c| c.total_distance).sum(),
            total_revenue,
            vat,
            car_rental: None,
            total_with_vat,
            // The band number; the route-days panel filters on it.
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

/// TAQA and Petromin per terminal, per car.
async fn rental_company_route_details(
    pool: &PgPool,
    company: &'static str,
    start_date: &str,
    end_date: &str,
    has_financial_access: bool,
) -> Result<Vec<RouteRevenueStats>> {
    let query = render(&format!(
        r#"
        WITH {cte}
        SELECT
            terminal,
            car_no_plate,
            ({{trip_count}})::bigint AS total_trips,
            COALESCE(SUM(tank_capacity), 0.0)::float8 AS total_volume,
            COALESCE(SUM(trip_distance), 0.0)::float8 AS total_distance,
            COUNT(DISTINCT date)::bigint              AS working_days,
            CASE WHEN $3 THEN COALESCE(SUM(revenue_base), 0.0)::float8 END     AS total_revenue,
            CASE WHEN $3 THEN COALESCE(SUM(allocated_rental), 0.0)::float8 END AS car_rental,
            CASE WHEN $3 THEN COALESCE(SUM(allocated_vat), 0.0)::float8 END    AS vat,
            CASE WHEN $3 THEN COALESCE(SUM(allocated_total), 0.0)::float8 END  AS total_with_vat
        FROM revenue
        GROUP BY terminal, car_no_plate
        ORDER BY terminal, car_no_plate
        "#,
        cte = company_window(company),
    ));

    let rows = sqlx::query(&query)
        .bind(start_date)
        .bind(end_date)
        .bind(has_financial_access)
        .fetch_all(pool)
        .await?;

    // The rate shown beside a terminal: the one in force at the end of the
    // range. A per-km rate is a price, so it is withheld below permission 3.
    let rate = if has_financial_access {
        per_km_rate_on(pool, company, end_date).await?
    } else {
        None
    };

    let mut terminal_stats: HashMap<String, Vec<CarStats>> = HashMap::new();
    for row in rows {
        let terminal: String = row.get("terminal");
        let car = CarStats {
            car_no_plate: row.get("car_no_plate"),
            total_trips: row.get("total_trips"),
            total_volume: row.get("total_volume"),
            total_distance: row.get("total_distance"),
            total_revenue: opt_f64(&row, "total_revenue"),
            working_days: row.get("working_days"),
            car_rental: opt_f64(&row, "car_rental"),
            vat: opt_f64(&row, "vat"),
            total_with_vat: opt_f64(&row, "total_with_vat"),
        };
        terminal_stats.entry(terminal).or_default().push(car);
    }

    let mut result = Vec::new();
    for (terminal, cars) in terminal_stats {
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
            total_trips: cars.iter().map(|c| c.total_trips).sum(),
            total_volume: cars.iter().map(|c| c.total_volume).sum(),
            total_distance: cars.iter().map(|c| c.total_distance).sum(),
            total_revenue,
            vat,
            car_rental,
            total_with_vat,
            fee: rate,
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
    let query = render(&format!(
        r#"
        WITH {cte}
        SELECT
            terminal,
            drop_off_point,
            MAX(fee_value)::float8 AS fee,
            car_no_plate,
            ({{trip_count}})::bigint AS total_trips,
            COALESCE(SUM(tank_capacity), 0.0)::float8 AS total_volume,
            COALESCE(SUM(trip_distance), 0.0)::float8 AS total_distance,
            COUNT(DISTINCT date)::bigint              AS working_days,
            CASE WHEN $3 THEN COALESCE(SUM(revenue_base), 0.0)::float8 END    AS total_revenue,
            CASE WHEN $3 THEN COALESCE(SUM(allocated_total), 0.0)::float8 END AS total_with_vat
        FROM revenue
        GROUP BY terminal, drop_off_point, car_no_plate
        ORDER BY terminal, drop_off_point, car_no_plate
        "#,
        cte = company_window("Petrol Arrows"),
    ));

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
        let car = CarStats {
            car_no_plate: row.get("car_no_plate"),
            total_trips: row.get("total_trips"),
            total_volume: row.get("total_volume"),
            total_distance: row.get("total_distance"),
            total_revenue: opt_f64(&row, "total_revenue"),
            working_days: row.get("working_days"),
            car_rental: None,
            vat: None,
            total_with_vat: opt_f64(&row, "total_with_vat"),
        };
        route_stats
            .entry((terminal, drop_off_point))
            .or_insert_with(|| (fee, Vec::new()))
            .1
            .push(car);
    }

    let mut result = Vec::new();
    for ((terminal, drop_off_point), (fee, cars)) in route_stats {
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
            total_trips: cars.iter().map(|c| c.total_trips).sum(),
            total_volume: cars.iter().map(|c| c.total_volume).sum(),
            total_distance: cars.iter().map(|c| c.total_distance).sum(),
            total_revenue,
            vat: None,
            car_rental: None,
            total_with_vat,
            // Petrol Arrows' fee is its price per 1,000 L.
            fee: fee_visible("Petrol Arrows", has_financial_access).then_some(fee),
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
    // `$3 IS NULL` is "every company", so one statement serves both shapes.
    let query = render(&format!(
        r#"
        WITH {cte},
        company_final AS (
            SELECT
                date,
                company,
                {{trip_count}} AS total_trips,
                COALESCE(SUM(tank_capacity), 0.0)::float8    AS total_volume,
                COALESCE(SUM(trip_distance), 0.0)::float8    AS total_distance,
                COALESCE(SUM(allocated_rental), 0.0)::float8 AS car_rental,
                COALESCE(SUM(allocated_vat), 0.0)::float8    AS vat,
                -- Base + the day's rental share + VAT on both.
                COALESCE(SUM(allocated_total), 0.0)::float8  AS total_revenue
            FROM revenue
            GROUP BY date, company
        ),
        date_totals AS (
            SELECT
                date,
                SUM(total_trips)::bigint    AS total_trips,
                SUM(total_volume)::float8   AS total_volume,
                SUM(total_distance)::float8 AS total_distance,
                SUM(total_revenue)::float8  AS total_revenue
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
            ) AS company_details
        FROM date_totals dt
        JOIN company_final cf ON dt.date = cf.date
        GROUP BY dt.date, dt.total_trips, dt.total_volume, dt.total_distance, dt.total_revenue
        ORDER BY dt.date ASC
        "#,
        cte = per_row_revenue_cte(
            "t.date BETWEEN $1 AND $2 AND ($3::text IS NULL OR t.company = $3)"
        ),
    ));

    let rows = sqlx::query(&query)
        .bind(start_date)
        .bind(end_date)
        .bind(company_filter)
        .fetch_all(pool)
        .await?;

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
                        total_revenue: has_financial_access.then_some(revenue),
                        vat: if has_financial_access { vat } else { None },
                        car_rental: if has_financial_access { car_rental } else { None },
                        total_with_vat: has_financial_access.then_some(revenue),
                    });
                }
            }
        }

        result.push(TripRevenueDateResponse {
            date,
            total_trips,
            total_volume,
            total_distance,
            total_revenue: has_financial_access.then_some(total_revenue),
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
/// limit, and the revenue it summed was `trip.revenue || trip.fee` — which for
/// Watanya fell through to a fee BAND NUMBER between 1 and 15 rather than money.
/// The money now comes from the stored revenue, like every other surface.
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
    let cte = per_row_revenue_cte("t.company = $1 AND t.date BETWEEN $2 AND $3");
    let sql = render(&format!(
        r#"
        WITH {cte}
        SELECT
            r.date,
            {{trip_count}}::bigint                            AS trips,
            COALESCE(SUM(r.tank_capacity), 0)::float8         AS volume,
            COALESCE(SUM(r.trip_distance), 0.0)::float8      AS distance,
            COALESCE(SUM(r.revenue_base), 0.0)::float8        AS revenue,
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
