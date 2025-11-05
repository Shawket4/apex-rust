use sqlx::{PgPool, Row};
use anyhow::Result;
use crate::models::expense::*;

// ============================================================================
// CRUD Operations
// ============================================================================

pub async fn create_expense(
    pool: &PgPool,
    expense: &CreateFleetExpense,
    user_id: i32,
) -> Result<FleetExpense> {
    // Validate payment method
    if expense.payment_method != "Cash" && expense.payment_method != "IPN Transfer" {
        return Err(anyhow::anyhow!("Invalid payment method. Must be 'Cash' or 'IPN Transfer'"));
    }
    
    // Validate amount
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
            id, car_no_plate, expense_date, expense_type, amount, description,
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

pub async fn get_expense_by_id(
    pool: &PgPool,
    id: i32,
) -> Result<Option<FleetExpense>> {
    let query = r#"
        SELECT 
            id, car_no_plate, expense_date, expense_type, amount, description,
            company, paid_by, payment_method, created_by, created_at, updated_at
        FROM fleet_expenses
        WHERE id = $1 AND deleted_at IS NULL
    "#;

    let row = sqlx::query(query)
        .bind(id)
        .fetch_optional(pool)
        .await?;

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

pub async fn list_expenses(
    pool: &PgPool,
    filters: &FleetExpenseFilters,
) -> Result<(Vec<FleetExpense>, i64)> {
    let page = filters.page.unwrap_or(1).max(1);
    let page_size = filters.page_size.unwrap_or(50).clamp(1, 100);
    let offset = (page - 1) * page_size;

    // FIXED: Create search_pattern at function scope to extend lifetime
    let search_pattern = filters.search.as_ref().map(|s| format!("%{}%", s));

    // Build WHERE clause dynamically
    let mut where_clauses = vec!["deleted_at IS NULL".to_string()];
    let mut bind_count = 1;

    if filters.start_date.is_some() {
        where_clauses.push(format!("expense_date >= ${}", bind_count));
        bind_count += 1;
    }

    if filters.end_date.is_some() {
        where_clauses.push(format!("expense_date <= ${}", bind_count));
        bind_count += 1;
    }

    if filters.car_no_plate.is_some() {
        where_clauses.push(format!("car_no_plate = ${}", bind_count));
        bind_count += 1;
    }

    if filters.company.is_some() {
        where_clauses.push(format!("company = ${}", bind_count));
        bind_count += 1;
    }

    if filters.expense_type.is_some() {
        where_clauses.push(format!("expense_type = ${}", bind_count));
        bind_count += 1;
    }

    if filters.payment_method.is_some() {
        where_clauses.push(format!("payment_method = ${}", bind_count));
        bind_count += 1;
    }

    if search_pattern.is_some() {
        where_clauses.push(format!(
            "(description ILIKE ${} OR paid_by ILIKE ${} OR car_no_plate ILIKE ${})",
            bind_count, bind_count, bind_count
        ));
        bind_count += 1;
    }

    let where_clause = where_clauses.join(" AND ");

    // Count query
    let count_query = format!(
        "SELECT COUNT(*) as total FROM fleet_expenses WHERE {}",
        where_clause
    );

    // Data query
    let data_query = format!(
        r#"
        SELECT 
            id, car_no_plate, expense_date, expense_type, amount, description,
            company, paid_by, payment_method, created_by, created_at, updated_at
        FROM fleet_expenses
        WHERE {}
        ORDER BY expense_date DESC, id DESC
        LIMIT ${} OFFSET ${}
        "#,
        where_clause, bind_count, bind_count + 1
    );

    // Build and execute count query
    let mut count_query_builder = sqlx::query(&count_query);
    
    if let Some(ref start_date) = filters.start_date {
        count_query_builder = count_query_builder.bind(start_date);
    }
    if let Some(ref end_date) = filters.end_date {
        count_query_builder = count_query_builder.bind(end_date);
    }
    if let Some(ref car) = filters.car_no_plate {
        count_query_builder = count_query_builder.bind(car);
    }
    if let Some(ref company) = filters.company {
        count_query_builder = count_query_builder.bind(company);
    }
    if let Some(ref expense_type) = filters.expense_type {
        count_query_builder = count_query_builder.bind(expense_type);
    }
    if let Some(ref payment_method) = filters.payment_method {
        count_query_builder = count_query_builder.bind(payment_method);
    }
    // FIXED: Now search_pattern lives long enough
    if let Some(ref pattern) = search_pattern {
        count_query_builder = count_query_builder.bind(pattern);
    }

    let total: i64 = count_query_builder
        .fetch_one(pool)
        .await?
        .get("total");

    // Build and execute data query
    let mut data_query_builder = sqlx::query(&data_query);
    
    if let Some(ref start_date) = filters.start_date {
        data_query_builder = data_query_builder.bind(start_date);
    }
    if let Some(ref end_date) = filters.end_date {
        data_query_builder = data_query_builder.bind(end_date);
    }
    if let Some(ref car) = filters.car_no_plate {
        data_query_builder = data_query_builder.bind(car);
    }
    if let Some(ref company) = filters.company {
        data_query_builder = data_query_builder.bind(company);
    }
    if let Some(ref expense_type) = filters.expense_type {
        data_query_builder = data_query_builder.bind(expense_type);
    }
    if let Some(ref payment_method) = filters.payment_method {
        data_query_builder = data_query_builder.bind(payment_method);
    }
    // FIXED: Now search_pattern lives long enough
    if let Some(ref pattern) = search_pattern {
        data_query_builder = data_query_builder.bind(pattern);
    }

    data_query_builder = data_query_builder.bind(page_size).bind(offset);

    let rows = data_query_builder.fetch_all(pool).await?;

    let expenses: Vec<FleetExpense> = rows
        .into_iter()
        .map(|r| FleetExpense {
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
        })
        .collect();

    Ok((expenses, total))
}

pub async fn update_expense(
    pool: &PgPool,
    id: i32,
    expense: &UpdateFleetExpense,
) -> Result<Option<FleetExpense>> {
    // Validate payment method if provided
    if let Some(ref method) = expense.payment_method {
        if method != "Cash" && method != "IPN Transfer" {
            return Err(anyhow::anyhow!("Invalid payment method. Must be 'Cash' or 'IPN Transfer'"));
        }
    }
    
    // Validate amount if provided
    if let Some(amount) = expense.amount {
        if amount < 0.0 {
            return Err(anyhow::anyhow!("Amount cannot be negative"));
        }
    }

    // Build UPDATE SET clause dynamically
    let mut set_clauses = Vec::new();
    let mut bind_count = 1;

    if expense.car_no_plate.is_some() {
        set_clauses.push(format!("car_no_plate = ${}", bind_count));
        bind_count += 1;
    }
    if expense.expense_date.is_some() {
        set_clauses.push(format!("expense_date = ${}", bind_count));
        bind_count += 1;
    }
    if expense.expense_type.is_some() {
        set_clauses.push(format!("expense_type = ${}", bind_count));
        bind_count += 1;
    }
    if expense.amount.is_some() {
        set_clauses.push(format!("amount = ${}", bind_count));
        bind_count += 1;
    }
    if expense.description.is_some() {
        set_clauses.push(format!("description = ${}", bind_count));
        bind_count += 1;
    }
    if expense.company.is_some() {
        set_clauses.push(format!("company = ${}", bind_count));
        bind_count += 1;
    }
    if expense.paid_by.is_some() {
        set_clauses.push(format!("paid_by = ${}", bind_count));
        bind_count += 1;
    }
    if expense.payment_method.is_some() {
        set_clauses.push(format!("payment_method = ${}", bind_count));
        bind_count += 1;
    }

    if set_clauses.is_empty() {
        // No fields to update, just return the current record
        return get_expense_by_id(pool, id).await;
    }

    let query = format!(
        r#"
        UPDATE fleet_expenses
        SET {}
        WHERE id = ${} AND deleted_at IS NULL
        RETURNING 
            id, car_no_plate, expense_date, expense_type, amount, description,
            company, paid_by, payment_method, created_by, created_at, updated_at
        "#,
        set_clauses.join(", "),
        bind_count
    );

    let mut query_builder = sqlx::query(&query);

    if let Some(ref car) = expense.car_no_plate {
        query_builder = query_builder.bind(car);
    }
    if let Some(ref date) = expense.expense_date {
        query_builder = query_builder.bind(date);
    }
    if let Some(ref exp_type) = expense.expense_type {
        query_builder = query_builder.bind(exp_type);
    }
    if let Some(amount) = expense.amount {
        query_builder = query_builder.bind(amount);
    }
    if let Some(ref desc) = expense.description {
        query_builder = query_builder.bind(desc);
    }
    if let Some(ref company) = expense.company {
        query_builder = query_builder.bind(company);
    }
    if let Some(ref paid_by) = expense.paid_by {
        query_builder = query_builder.bind(paid_by);
    }
    if let Some(ref method) = expense.payment_method {
        query_builder = query_builder.bind(method);
    }

    query_builder = query_builder.bind(id);

    let row = query_builder.fetch_optional(pool).await?;

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

pub async fn delete_expense(
    pool: &PgPool,
    id: i32,
) -> Result<bool> {
    let query = r#"
        UPDATE fleet_expenses
        SET deleted_at = CURRENT_TIMESTAMP
        WHERE id = $1 AND deleted_at IS NULL
    "#;

    let result = sqlx::query(query)
        .bind(id)
        .execute(pool)
        .await?;

    Ok(result.rows_affected() > 0)
}

// ============================================================================
// Statistics Operations
// ============================================================================

pub async fn get_expense_statistics(
    pool: &PgPool,
    filters: &FleetExpenseFilters,
) -> Result<ExpenseStatistics> {
    // Build WHERE clause
    let mut where_clauses = vec!["deleted_at IS NULL".to_string()];
    
    if filters.start_date.is_some() {
        where_clauses.push("expense_date >= $1".to_string());
    }
    if filters.end_date.is_some() {
        let param_num = if filters.start_date.is_some() { 2 } else { 1 };
        where_clauses.push(format!("expense_date <= ${}", param_num));
    }
    
    let where_clause = where_clauses.join(" AND ");

    // Overall totals
    let totals_query = format!(
        "SELECT COALESCE(SUM(amount), 0.0)::float8 as total, COUNT(*)::bigint as count FROM fleet_expenses WHERE {}",
        where_clause
    );

    let mut totals_builder = sqlx::query(&totals_query);
    if let Some(ref start) = filters.start_date {
        totals_builder = totals_builder.bind(start);
    }
    if let Some(ref end) = filters.end_date {
        totals_builder = totals_builder.bind(end);
    }

    let totals_row = totals_builder.fetch_one(pool).await?;
    let total_amount: f64 = totals_row.get("total");
    let expense_count: i64 = totals_row.get("count");

    // By expense type
    let by_type_query = format!(
        r#"
        SELECT 
            expense_type,
            COALESCE(SUM(amount), 0.0)::float8 as total,
            COUNT(*)::bigint as count
        FROM fleet_expenses
        WHERE {}
        GROUP BY expense_type
        ORDER BY total DESC
        "#,
        where_clause
    );

    let mut by_type_builder = sqlx::query(&by_type_query);
    if let Some(ref start) = filters.start_date {
        by_type_builder = by_type_builder.bind(start);
    }
    if let Some(ref end) = filters.end_date {
        by_type_builder = by_type_builder.bind(end);
    }

    let by_type_rows = by_type_builder.fetch_all(pool).await?;
    let by_type: Vec<ExpenseByType> = by_type_rows
        .into_iter()
        .map(|r| ExpenseByType {
            expense_type: r.get("expense_type"),
            total_amount: r.get("total"),
            count: r.get("count"),
        })
        .collect();

    // By company
    let by_company_query = format!(
        r#"
        SELECT 
            COALESCE(company, 'General') as company,
            COALESCE(SUM(amount), 0.0)::float8 as total,
            COUNT(*)::bigint as count
        FROM fleet_expenses
        WHERE {}
        GROUP BY company
        ORDER BY total DESC
        "#,
        where_clause
    );

    let mut by_company_builder = sqlx::query(&by_company_query);
    if let Some(ref start) = filters.start_date {
        by_company_builder = by_company_builder.bind(start);
    }
    if let Some(ref end) = filters.end_date {
        by_company_builder = by_company_builder.bind(end);
    }

    let by_company_rows = by_company_builder.fetch_all(pool).await?;
    let by_company: Vec<ExpenseByCompany> = by_company_rows
        .into_iter()
        .map(|r| ExpenseByCompany {
            company: r.get("company"),
            total_amount: r.get("total"),
            count: r.get("count"),
        })
        .collect();

    // By car
    let by_car_query = format!(
        r#"
        SELECT 
            car_no_plate,
            COALESCE(SUM(amount), 0.0)::float8 as total,
            COUNT(*)::bigint as count
        FROM fleet_expenses
        WHERE {} AND car_no_plate IS NOT NULL
        GROUP BY car_no_plate
        ORDER BY total DESC
        "#,
        where_clause
    );

    let mut by_car_builder = sqlx::query(&by_car_query);
    if let Some(ref start) = filters.start_date {
        by_car_builder = by_car_builder.bind(start);
    }
    if let Some(ref end) = filters.end_date {
        by_car_builder = by_car_builder.bind(end);
    }

    let by_car_rows = by_car_builder.fetch_all(pool).await?;
    let by_car: Vec<ExpenseByCar> = by_car_rows
        .into_iter()
        .map(|r| ExpenseByCar {
            car_no_plate: r.get("car_no_plate"),
            total_amount: r.get("total"),
            count: r.get("count"),
        })
        .collect();

    // By payment method
    let by_method_query = format!(
        r#"
        SELECT 
            payment_method,
            COALESCE(SUM(amount), 0.0)::float8 as total,
            COUNT(*)::bigint as count
        FROM fleet_expenses
        WHERE {}
        GROUP BY payment_method
        ORDER BY total DESC
        "#,
        where_clause
    );

    let mut by_method_builder = sqlx::query(&by_method_query);
    if let Some(ref start) = filters.start_date {
        by_method_builder = by_method_builder.bind(start);
    }
    if let Some(ref end) = filters.end_date {
        by_method_builder = by_method_builder.bind(end);
    }

    let by_method_rows = by_method_builder.fetch_all(pool).await?;
    let by_payment_method: Vec<ExpenseByPaymentMethod> = by_method_rows
        .into_iter()
        .map(|r| ExpenseByPaymentMethod {
            payment_method: r.get("payment_method"),
            total_amount: r.get("total"),
            count: r.get("count"),
        })
        .collect();

    // By date
    let by_date_query = format!(
        r#"
        SELECT 
            expense_date::text as date,
            COALESCE(SUM(amount), 0.0)::float8 as total,
            COUNT(*)::bigint as count
        FROM fleet_expenses
        WHERE {}
        GROUP BY expense_date
        ORDER BY expense_date DESC
        "#,
        where_clause
    );

    let mut by_date_builder = sqlx::query(&by_date_query);
    if let Some(ref start) = filters.start_date {
        by_date_builder = by_date_builder.bind(start);
    }
    if let Some(ref end) = filters.end_date {
        by_date_builder = by_date_builder.bind(end);
    }

    let by_date_rows = by_date_builder.fetch_all(pool).await?;
    let by_date: Vec<ExpenseByDate> = by_date_rows
        .into_iter()
        .map(|r| ExpenseByDate {
            date: r.get("date"),
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
    })
}