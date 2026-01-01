use sqlx::{PgPool, Row};
use anyhow::Result;
use chrono::NaiveDate;
use crate::models::expense::*;

// ============================================================================
// Unified Expense Query - Combines all sources
// ============================================================================

pub async fn list_unified_expenses(
    pool: &PgPool,
    filters: &FleetExpenseFilters,
) -> Result<(Vec<UnifiedExpense>, i64, SourceCounts)> {
    let page = filters.page.unwrap_or(1).max(1);
    let page_size = filters.page_size.unwrap_or(50).clamp(1, 100);
    let offset = (page - 1) * page_size;

    let include_fuel = filters.include_fuel.unwrap_or(true);
    let include_loans = filters.include_loans.unwrap_or(true);
    
    let search_pattern = filters.search.as_ref().map(|s| format!("%{}%", s));

    // Build date filter strings
    let date_filter = build_date_filter(&filters.start_date, &filters.end_date);
    
    // Build the UNION ALL query
    let mut union_parts = Vec::new();
    
    // Fleet expenses part
    union_parts.push(format!(r#"
        SELECT 
            id,
            'fleet_expense' as source,
            car_no_plate,
            expense_date,
            expense_type,
            amount::float8 as amount,
            description,
            company,
            paid_by,
            payment_method,
            created_by,
            created_at,
            updated_at,
            NULL::float8 as liters,
            NULL::float8 as price_per_liter,
            NULL::text as driver_name,
            NULL::bigint as odometer_before,
            NULL::bigint as odometer_after,
            NULL::boolean as is_paid,
            NULL::bigint as driver_id,
            NULL::bigint as employee_id
        FROM fleet_expenses
        WHERE deleted_at IS NULL
        {}
        {}
        {}
        {}
    "#,
        date_filter.replace("expense_date", "expense_date"),
        filters.car_no_plate.as_ref().map(|_| "AND car_no_plate = $1").unwrap_or(""),
        filters.company.as_ref().map(|_| "AND company = $2").unwrap_or(""),
        filters.expense_type.as_ref().map(|t| {
            if t == "Fuel" || t == "Loan" { "AND 1=0".to_string() } // Exclude if filtering for fuel/loan
            else { format!("AND expense_type = '{}'", t.replace("'", "''")) }
        }).unwrap_or_default()
    ));

    // Fuel events part (if included and not filtered out)
    if include_fuel && filters.expense_type.as_ref().map(|t| t == "Fuel" || t.is_empty()).unwrap_or(true) {
        union_parts.push(format!(r#"
            SELECT 
                id,
                'fuel_event' as source,
                car_no_plate,
                CASE 
                    WHEN date ~ '^\d{{4}}-\d{{2}}-\d{{2}}' THEN date::date
                    ELSE CURRENT_DATE
                END as expense_date,
                'Fuel' as expense_type,
                COALESCE(price, 0)::float8 as amount,
                CONCAT('Fuel: ', COALESCE(liters::text, '0'), 'L @ ', COALESCE(price_per_liter::text, '0'), '/L') as description,
                transporter as company,
                driver_name as paid_by,
                COALESCE(method, 'Cash') as payment_method,
                NULL::integer as created_by,
                COALESCE(created_at, CURRENT_TIMESTAMP) as created_at,
                COALESCE(updated_at, CURRENT_TIMESTAMP) as updated_at,
                liters::float8 as liters,
                price_per_liter::float8 as price_per_liter,
                driver_name,
                odometer_before,
                odometer_after,
                NULL::boolean as is_paid,
                NULL::bigint as driver_id,
                NULL::bigint as employee_id
            FROM fuel_events
            WHERE deleted_at IS NULL
            {}
            {}
        "#,
            build_fuel_date_filter(&filters.start_date, &filters.end_date),
            filters.car_no_plate.as_ref().map(|_| "AND car_no_plate = $1").unwrap_or("")
        ));
    }

    // Loans part (if included and not filtered out)
    if include_loans && filters.expense_type.as_ref().map(|t| t == "Loan" || t.is_empty()).unwrap_or(true) {
        union_parts.push(format!(r#"
            SELECT 
                id,
                'loan' as source,
                NULL as car_no_plate,
                CASE 
                    WHEN date ~ '^\d{{4}}-\d{{2}}-\d{{2}}' THEN date::date
                    ELSE CURRENT_DATE
                END as expense_date,
                'Loan' as expense_type,
                COALESCE(amount, 0)::float8 as amount,
                description,
                NULL as company,
                NULL as paid_by,
                COALESCE(method, 'Cash') as payment_method,
                NULL::integer as created_by,
                COALESCE(created_at, CURRENT_TIMESTAMP) as created_at,
                COALESCE(updated_at, CURRENT_TIMESTAMP) as updated_at,
                NULL::float8 as liters,
                NULL::float8 as price_per_liter,
                NULL::text as driver_name,
                NULL::bigint as odometer_before,
                NULL::bigint as odometer_after,
                is_paid,
                driver_id,
                employee_id
            FROM loans
            WHERE deleted_at IS NULL
            {}
        "#,
            build_loan_date_filter(&filters.start_date, &filters.end_date)
        ));
    }

    let union_query = union_parts.join(" UNION ALL ");
    
    // Wrap in CTE for counting and pagination
    let full_query = format!(r#"
        WITH unified AS ({})
        SELECT * FROM unified
        ORDER BY expense_date DESC, created_at DESC
        LIMIT {} OFFSET {}
    "#, union_query, page_size, offset);

    let count_query = format!(r#"
        WITH unified AS ({})
        SELECT COUNT(*) as total FROM unified
    "#, union_query);

    // Execute count query
    let total: i64 = sqlx::query(&count_query)
        .fetch_one(pool)
        .await?
        .get("total");

    // Execute data query
    let rows = sqlx::query(&full_query)
        .fetch_all(pool)
        .await?;

    let expenses: Vec<UnifiedExpense> = rows
        .into_iter()
        .map(|r| {
            let source_str: String = r.get("source");
            let source = match source_str.as_str() {
                "fuel_event" => ExpenseSource::FuelEvent,
                "loan" => ExpenseSource::Loan,
                _ => ExpenseSource::FleetExpense,
            };
            
            UnifiedExpense {
                id: r.get("id"),
                source,
                car_no_plate: r.get("car_no_plate"),
                expense_date: r.get("expense_date"),
                expense_type: r.get("expense_type"),
                amount: r.get("amount"),
                description: r.get("description"),
                company: r.get("company"),
                paid_by: r.get("paid_by"),
                payment_method: r.get("payment_method"),
                created_by: r.get("created_by"),
                created_at: r.get("created_at"),
                updated_at: r.get("updated_at"),
                liters: r.get("liters"),
                price_per_liter: r.get("price_per_liter"),
                driver_name: r.get("driver_name"),
                odometer_before: r.get("odometer_before"),
                odometer_after: r.get("odometer_after"),
                is_paid: r.get("is_paid"),
                driver_id: r.get("driver_id"),
                employee_id: r.get("employee_id"),
            }
        })
        .collect();

    // Get source counts
    let source_counts = get_source_counts(pool, filters).await?;

    Ok((expenses, total, source_counts))
}

async fn get_source_counts(pool: &PgPool, filters: &FleetExpenseFilters) -> Result<SourceCounts> {
    let date_filter_fleet = build_date_filter(&filters.start_date, &filters.end_date);
    let date_filter_fuel = build_fuel_date_filter(&filters.start_date, &filters.end_date);
    let date_filter_loan = build_loan_date_filter(&filters.start_date, &filters.end_date);

    let fleet_count: i64 = sqlx::query(&format!(
        "SELECT COUNT(*) as cnt FROM fleet_expenses WHERE deleted_at IS NULL {}",
        date_filter_fleet
    ))
        .fetch_one(pool)
        .await?
        .get("cnt");

    let fuel_count: i64 = sqlx::query(&format!(
        "SELECT COUNT(*) as cnt FROM fuel_events WHERE deleted_at IS NULL {}",
        date_filter_fuel
    ))
        .fetch_one(pool)
        .await?
        .get("cnt");

    let loan_count: i64 = sqlx::query(&format!(
        "SELECT COUNT(*) as cnt FROM loans WHERE deleted_at IS NULL {}",
        date_filter_loan
    ))
        .fetch_one(pool)
        .await?
        .get("cnt");

    Ok(SourceCounts {
        fleet_expenses: fleet_count,
        fuel_events: fuel_count,
        loans: loan_count,
        total: fleet_count + fuel_count + loan_count,
    })
}

fn build_date_filter(start: &Option<NaiveDate>, end: &Option<NaiveDate>) -> String {
    let mut parts = Vec::new();
    if let Some(s) = start {
        parts.push(format!("AND expense_date >= '{}'", s));
    }
    if let Some(e) = end {
        parts.push(format!("AND expense_date <= '{}'", e));
    }
    parts.join(" ")
}

fn build_fuel_date_filter(start: &Option<NaiveDate>, end: &Option<NaiveDate>) -> String {
    let mut parts = Vec::new();
    if let Some(s) = start {
        parts.push(format!("AND date >= '{}'", s));
    }
    if let Some(e) = end {
        parts.push(format!("AND date <= '{}'", e));
    }
    parts.join(" ")
}

fn build_loan_date_filter(start: &Option<NaiveDate>, end: &Option<NaiveDate>) -> String {
    let mut parts = Vec::new();
    if let Some(s) = start {
        parts.push(format!("AND date >= '{}'", s));
    }
    if let Some(e) = end {
        parts.push(format!("AND date <= '{}'", e));
    }
    parts.join(" ")
}

// ============================================================================
// Unified Statistics
// ============================================================================

pub async fn get_unified_expense_statistics(
    pool: &PgPool,
    filters: &FleetExpenseFilters,
) -> Result<ExpenseStatistics> {
    let date_filter_fleet = build_date_filter(&filters.start_date, &filters.end_date);
    let date_filter_fuel = build_fuel_date_filter(&filters.start_date, &filters.end_date);
    let date_filter_loan = build_loan_date_filter(&filters.start_date, &filters.end_date);

    // Build unified CTE for statistics
    let unified_cte = format!(r#"
        WITH unified AS (
            SELECT amount::float8 as amount, expense_type, company, car_no_plate, 
                   payment_method, expense_date::text as date_str, 'fleet_expense' as source
            FROM fleet_expenses WHERE deleted_at IS NULL {}
            UNION ALL
            SELECT COALESCE(price, 0)::float8 as amount, 'Fuel' as expense_type, 
                   transporter as company, car_no_plate, COALESCE(method, 'Cash') as payment_method,
                   date as date_str, 'fuel_event' as source
            FROM fuel_events WHERE deleted_at IS NULL {}
            UNION ALL
            SELECT COALESCE(amount, 0)::float8 as amount, 'Loan' as expense_type,
                   NULL as company, NULL as car_no_plate, COALESCE(method, 'Cash') as payment_method,
                   date as date_str, 'loan' as source
            FROM loans WHERE deleted_at IS NULL {}
        )
    "#, date_filter_fleet, date_filter_fuel, date_filter_loan);

    // Total amount and count
    let totals_query = format!("{} SELECT COALESCE(SUM(amount), 0)::float8 as total, COUNT(*)::bigint as count FROM unified", unified_cte);
    let totals_row = sqlx::query(&totals_query).fetch_one(pool).await?;
    let total_amount: f64 = totals_row.get("total");
    let expense_count: i64 = totals_row.get("count");

    // By type
    let by_type_query = format!("{} SELECT expense_type, COALESCE(SUM(amount), 0)::float8 as total, COUNT(*)::bigint as count FROM unified GROUP BY expense_type ORDER BY total DESC", unified_cte);
    let by_type: Vec<ExpenseByType> = sqlx::query(&by_type_query)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| ExpenseByType {
            expense_type: r.get("expense_type"),
            total_amount: r.get("total"),
            count: r.get("count"),
        })
        .collect();

    // By company
    let by_company_query = format!("{} SELECT COALESCE(company, 'General') as company, COALESCE(SUM(amount), 0)::float8 as total, COUNT(*)::bigint as count FROM unified GROUP BY company ORDER BY total DESC", unified_cte);
    let by_company: Vec<ExpenseByCompany> = sqlx::query(&by_company_query)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| ExpenseByCompany {
            company: r.get("company"),
            total_amount: r.get("total"),
            count: r.get("count"),
        })
        .collect();

    // By car
    let by_car_query = format!("{} SELECT car_no_plate, COALESCE(SUM(amount), 0)::float8 as total, COUNT(*)::bigint as count FROM unified WHERE car_no_plate IS NOT NULL GROUP BY car_no_plate ORDER BY total DESC", unified_cte);
    let by_car: Vec<ExpenseByCar> = sqlx::query(&by_car_query)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| ExpenseByCar {
            car_no_plate: r.get("car_no_plate"),
            total_amount: r.get("total"),
            count: r.get("count"),
        })
        .collect();

    // By payment method
    let by_method_query = format!("{} SELECT COALESCE(payment_method, 'Unknown') as payment_method, COALESCE(SUM(amount), 0)::float8 as total, COUNT(*)::bigint as count FROM unified GROUP BY payment_method ORDER BY total DESC", unified_cte);
    let by_payment_method: Vec<ExpenseByPaymentMethod> = sqlx::query(&by_method_query)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| ExpenseByPaymentMethod {
            payment_method: r.get("payment_method"),
            total_amount: r.get("total"),
            count: r.get("count"),
        })
        .collect();

    // By date
    let by_date_query = format!("{} SELECT date_str as date, COALESCE(SUM(amount), 0)::float8 as total, COUNT(*)::bigint as count FROM unified WHERE date_str IS NOT NULL GROUP BY date_str ORDER BY date_str DESC", unified_cte);
    let by_date: Vec<ExpenseByDate> = sqlx::query(&by_date_query)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| ExpenseByDate {
            date: r.get("date"),
            total_amount: r.get("total"),
            count: r.get("count"),
        })
        .collect();

    // By source
    let by_source_query = format!("{} SELECT source, COALESCE(SUM(amount), 0)::float8 as total, COUNT(*)::bigint as count FROM unified GROUP BY source ORDER BY total DESC", unified_cte);
    let by_source: Vec<ExpenseBySource> = sqlx::query(&by_source_query)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| ExpenseBySource {
            source: r.get("source"),
            total_amount: r.get("total"),
            count: r.get("count"),
        })
        .collect();

    Ok(ExpenseStatistics {
        total_amount,
        expense_count,
        by_type,
        by_company,
        by_car,
        by_payment_method,
        by_date,
        by_source,
    })
}

// ============================================================================
// Original CRUD Operations (kept for fleet_expenses table only)
// ============================================================================

pub async fn create_expense(
    pool: &PgPool,
    expense: &CreateFleetExpense,
    user_id: i32,
) -> Result<FleetExpense> {
    if expense.payment_method != "Cash" && expense.payment_method != "IPN Transfer" {
        return Err(anyhow::anyhow!("Invalid payment method. Must be 'Cash' or 'IPN Transfer'"));
    }
    if expense.amount < 0.0 {
        return Err(anyhow::anyhow!("Amount cannot be negative"));
    }

    let query = r#"
        INSERT INTO fleet_expenses (
            car_no_plate, expense_date, expense_type, amount, description,
            company, paid_by, payment_method, created_by
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        RETURNING 
            id, car_no_plate, expense_date, expense_type, amount::float8 as amount, description,
            company, paid_by, payment_method, created_by, created_at, updated_at
    "#;

    let row = sqlx::query(query)
        .bind(&expense.car_no_plate)
        .bind(&expense.expense_date)
        .bind(&expense.expense_type)
        .bind(expense.amount)
        .bind(&expense.description)
        .bind(&expense.company)
        .bind(&expense.paid_by)
        .bind(&expense.payment_method)
        .bind(user_id)
        .fetch_one(pool)
        .await?;

    Ok(FleetExpense {
        id: row.get("id"),
        car_no_plate: row.get("car_no_plate"),
        expense_date: row.get("expense_date"),
        expense_type: row.get("expense_type"),
        amount: row.get("amount"),
        description: row.get("description"),
        company: row.get("company"),
        paid_by: row.get("paid_by"),
        payment_method: row.get("payment_method"),
        created_by: row.get("created_by"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

pub async fn get_expense_by_id(pool: &PgPool, id: i32) -> Result<Option<FleetExpense>> {
    let query = r#"
        SELECT id, car_no_plate, expense_date, expense_type, amount::float8 as amount, description,
               company, paid_by, payment_method, created_by, created_at, updated_at
        FROM fleet_expenses
        WHERE id = $1 AND deleted_at IS NULL
    "#;

    let row = sqlx::query(query).bind(id).fetch_optional(pool).await?;

    Ok(row.map(|r| FleetExpense {
        id: r.get("id"),
        car_no_plate: r.get("car_no_plate"),
        expense_date: r.get("expense_date"),
        expense_type: r.get("expense_type"),
        amount: r.get("amount"),
        description: r.get("description"),
        company: r.get("company"),
        paid_by: r.get("paid_by"),
        payment_method: r.get("payment_method"),
        created_by: r.get("created_by"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }))
}

pub async fn update_expense(pool: &PgPool, id: i32, expense: &UpdateFleetExpense) -> Result<Option<FleetExpense>> {
    if let Some(ref method) = expense.payment_method {
        if method != "Cash" && method != "IPN Transfer" {
            return Err(anyhow::anyhow!("Invalid payment method"));
        }
    }
    if let Some(amount) = expense.amount {
        if amount < 0.0 {
            return Err(anyhow::anyhow!("Amount cannot be negative"));
        }
    }

    let mut set_clauses = Vec::new();
    let mut bind_count = 1;

    if expense.car_no_plate.is_some() { set_clauses.push(format!("car_no_plate = ${}", bind_count)); bind_count += 1; }
    if expense.expense_date.is_some() { set_clauses.push(format!("expense_date = ${}", bind_count)); bind_count += 1; }
    if expense.expense_type.is_some() { set_clauses.push(format!("expense_type = ${}", bind_count)); bind_count += 1; }
    if expense.amount.is_some() { set_clauses.push(format!("amount = ${}", bind_count)); bind_count += 1; }
    if expense.description.is_some() { set_clauses.push(format!("description = ${}", bind_count)); bind_count += 1; }
    if expense.company.is_some() { set_clauses.push(format!("company = ${}", bind_count)); bind_count += 1; }
    if expense.paid_by.is_some() { set_clauses.push(format!("paid_by = ${}", bind_count)); bind_count += 1; }
    if expense.payment_method.is_some() { set_clauses.push(format!("payment_method = ${}", bind_count)); bind_count += 1; }

    if set_clauses.is_empty() {
        return get_expense_by_id(pool, id).await;
    }

    let query = format!(
        r#"UPDATE fleet_expenses SET {} WHERE id = ${} AND deleted_at IS NULL
           RETURNING id, car_no_plate, expense_date, expense_type, amount::float8 as amount, 
                     description, company, paid_by, payment_method, created_by, created_at, updated_at"#,
        set_clauses.join(", "), bind_count
    );

    let mut qb = sqlx::query(&query);
    if let Some(ref v) = expense.car_no_plate { qb = qb.bind(v); }
    if let Some(ref v) = expense.expense_date { qb = qb.bind(v); }
    if let Some(ref v) = expense.expense_type { qb = qb.bind(v); }
    if let Some(v) = expense.amount { qb = qb.bind(v); }
    if let Some(ref v) = expense.description { qb = qb.bind(v); }
    if let Some(ref v) = expense.company { qb = qb.bind(v); }
    if let Some(ref v) = expense.paid_by { qb = qb.bind(v); }
    if let Some(ref v) = expense.payment_method { qb = qb.bind(v); }
    qb = qb.bind(id);

    let row = qb.fetch_optional(pool).await?;
    Ok(row.map(|r| FleetExpense {
        id: r.get("id"), car_no_plate: r.get("car_no_plate"), expense_date: r.get("expense_date"),
        expense_type: r.get("expense_type"), amount: r.get("amount"), description: r.get("description"),
        company: r.get("company"), paid_by: r.get("paid_by"), payment_method: r.get("payment_method"),
        created_by: r.get("created_by"), created_at: r.get("created_at"), updated_at: r.get("updated_at"),
    }))
}

pub async fn delete_expense(pool: &PgPool, id: i32) -> Result<bool> {
    let result = sqlx::query("UPDATE fleet_expenses SET deleted_at = CURRENT_TIMESTAMP WHERE id = $1 AND deleted_at IS NULL")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}