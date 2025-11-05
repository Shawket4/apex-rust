use actix_web::{web, HttpRequest, HttpResponse, HttpMessage};
use sqlx::PgPool;
use anyhow::Result;

use crate::auth::Claims;
use crate::models::expense::*;
use crate::db::expense_queries::*;
use crate::utils::response;

// ============================================================================
// Query Parameters
// ============================================================================

#[derive(serde::Deserialize)]
pub struct FormatQuery {
    pub format: Option<String>,
}

// ============================================================================
// Helper Functions
// ============================================================================

fn check_financial_access(claims: &Claims) -> Result<(), actix_web::Error> {
    let permission = claims.permission.unwrap_or(0);
    
    if !claims.has_permission(3, permission) {
        return Err(actix_web::error::ErrorForbidden(
            "Financial access required (permission level >= 3)"
        ));
    }
    
    Ok(())
}

fn extract_user_id(req: &HttpRequest) -> Result<i32, actix_web::Error> {
    let claims = req
        .extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Authentication required"))?;
    
    claims
        .user_id
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("User ID not found in token"))
}

// ============================================================================
// CRUD Handlers
// ============================================================================

pub async fn create_expense_handler(
    pool: web::Data<PgPool>,
    expense_data: web::Json<CreateFleetExpense>,
    query: web::Query<FormatQuery>,
    req: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    // Check authentication and permission
    let claims = req
        .extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Authentication required"))?;
    
    check_financial_access(&claims)?;
    
    let user_id = extract_user_id(&req)?;

    // Create expense
    let expense = create_expense(pool.get_ref(), &expense_data, user_id)
        .await
        .map_err(|e| actix_web::error::ErrorBadRequest(format!("Failed to create expense: {}", e)))?;

    let response_data = FleetExpenseSingleResponse {
        message: "Expense created successfully".to_string(),
        data: expense,
    };

    let use_msgpack = query.format.as_deref() == Some("msgpack");
    response(&response_data, use_msgpack)
        .map_err(actix_web::error::ErrorInternalServerError)
}

pub async fn get_expense_handler(
    pool: web::Data<PgPool>,
    path: web::Path<i32>,
    query: web::Query<FormatQuery>,
    req: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    // Check authentication and permission
    let claims = req
        .extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Authentication required"))?;
    
    check_financial_access(&claims)?;

    let expense_id = path.into_inner();
    
    let expense = get_expense_by_id(pool.get_ref(), expense_id)
        .await
        .map_err(actix_web::error::ErrorInternalServerError)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Expense not found"))?;

    let response_data = FleetExpenseSingleResponse {
        message: "Expense retrieved successfully".to_string(),
        data: expense,
    };

    let use_msgpack = query.format.as_deref() == Some("msgpack");
    response(&response_data, use_msgpack)
        .map_err(actix_web::error::ErrorInternalServerError)
}

pub async fn list_expenses_handler(
    pool: web::Data<PgPool>,
    query: web::Query<FleetExpenseFilters>,
    req: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    // Check authentication and permission
    let claims = req
        .extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Authentication required"))?;
    
    check_financial_access(&claims)?;

    let use_msgpack = query.format.as_deref() == Some("msgpack");

    let (expenses, total) = list_expenses(pool.get_ref(), &query)
        .await
        .map_err(actix_web::error::ErrorInternalServerError)?;

    let page = query.page.unwrap_or(1).max(1);
    let page_size = query.page_size.unwrap_or(50).clamp(1, 100);
    let total_pages = (total as f64 / page_size as f64).ceil() as i64;

    let response_data = FleetExpenseListResponse {
        message: "Expenses retrieved successfully".to_string(),
        data: expenses,
        pagination: PaginationInfo {
            page,
            page_size,
            total_records: total,
            total_pages,
        },
    };

    response(&response_data, use_msgpack)
        .map_err(actix_web::error::ErrorInternalServerError)
}

pub async fn update_expense_handler(
    pool: web::Data<PgPool>,
    path: web::Path<i32>,
    expense_data: web::Json<UpdateFleetExpense>,
    query: web::Query<FormatQuery>,
    req: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    // Check authentication and permission
    let claims = req
        .extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Authentication required"))?;
    
    check_financial_access(&claims)?;

    let expense_id = path.into_inner();

    let expense = update_expense(pool.get_ref(), expense_id, &expense_data)
        .await
        .map_err(|e| actix_web::error::ErrorBadRequest(format!("Failed to update expense: {}", e)))?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Expense not found"))?;

    let response_data = FleetExpenseSingleResponse {
        message: "Expense updated successfully".to_string(),
        data: expense,
    };

    let use_msgpack = query.format.as_deref() == Some("msgpack");
    response(&response_data, use_msgpack)
        .map_err(actix_web::error::ErrorInternalServerError)
}

pub async fn delete_expense_handler(
    pool: web::Data<PgPool>,
    path: web::Path<i32>,
    query: web::Query<FormatQuery>,
    req: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    // Check authentication and permission
    let claims = req
        .extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Authentication required"))?;
    
    check_financial_access(&claims)?;

    let expense_id = path.into_inner();

    let deleted = delete_expense(pool.get_ref(), expense_id)
        .await
        .map_err(actix_web::error::ErrorInternalServerError)?;

    if !deleted {
        return Err(actix_web::error::ErrorNotFound("Expense not found"));
    }

    let response_data = FleetExpenseDeleteResponse {
        message: "Expense deleted successfully".to_string(),
        id: expense_id,
    };

    let use_msgpack = query.format.as_deref() == Some("msgpack");
    response(&response_data, use_msgpack)
        .map_err(actix_web::error::ErrorInternalServerError)
}

// ============================================================================
// Statistics Handler
// ============================================================================

pub async fn get_expense_statistics_handler(
    pool: web::Data<PgPool>,
    query: web::Query<FleetExpenseFilters>,
    req: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    // Check authentication and permission
    let claims = req
        .extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Authentication required"))?;
    
    check_financial_access(&claims)?;

    let use_msgpack = query.format.as_deref() == Some("msgpack");

    let statistics = get_expense_statistics(pool.get_ref(), &query)
        .await
        .map_err(actix_web::error::ErrorInternalServerError)?;

    let response_data = ExpenseStatisticsResponse {
        message: "Expense statistics retrieved successfully".to_string(),
        data: statistics,
    };

    response(&response_data, use_msgpack)
        .map_err(actix_web::error::ErrorInternalServerError)
}