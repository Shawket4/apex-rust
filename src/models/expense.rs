use serde::{Deserialize, Serialize};
use chrono::{NaiveDate, NaiveDateTime};

// ============================================================================
// Fleet Expense Models
// ============================================================================

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FleetExpense {
    pub id: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub car_no_plate: Option<String>,
    pub expense_date: NaiveDate,
    pub expense_type: String,
    pub amount: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub company: Option<String>,
    pub paid_by: String,
    pub payment_method: String,
    pub created_by: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

#[derive(Debug, Deserialize)]
pub struct CreateFleetExpense {
    pub car_no_plate: Option<String>,
    pub expense_date: NaiveDate,
    pub expense_type: String,
    pub amount: f64,
    pub description: Option<String>,
    pub company: Option<String>,
    pub paid_by: String,
    pub payment_method: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateFleetExpense {
    pub car_no_plate: Option<String>,
    pub expense_date: Option<NaiveDate>,
    pub expense_type: Option<String>,
    pub amount: Option<f64>,
    pub description: Option<String>,
    pub company: Option<String>,
    pub paid_by: Option<String>,
    pub payment_method: Option<String>,
}

// ============================================================================
// Filter Models
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct FleetExpenseFilters {
    pub start_date: Option<NaiveDate>,
    pub end_date: Option<NaiveDate>,
    pub car_no_plate: Option<String>,
    pub company: Option<String>,
    pub expense_type: Option<String>,
    pub payment_method: Option<String>,
    pub search: Option<String>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
    pub format: Option<String>,
}

// ============================================================================
// Response Models
// ============================================================================

#[derive(Debug, Serialize, Deserialize)]
pub struct FleetExpenseListResponse {
    pub message: String,
    pub data: Vec<FleetExpense>,
    pub pagination: PaginationInfo,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FleetExpenseSingleResponse {
    pub message: String,
    pub data: FleetExpense,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FleetExpenseDeleteResponse {
    pub message: String,
    pub id: i32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PaginationInfo {
    pub page: i64,
    pub page_size: i64,
    pub total_records: i64,
    pub total_pages: i64,
}

// ============================================================================
// Statistics Models
// ============================================================================

#[derive(Debug, Serialize, Deserialize)]
pub struct ExpenseStatistics {
    pub total_amount: f64,
    pub expense_count: i64,
    pub by_type: Vec<ExpenseByType>,
    pub by_company: Vec<ExpenseByCompany>,
    pub by_car: Vec<ExpenseByCar>,
    pub by_payment_method: Vec<ExpenseByPaymentMethod>,
    pub by_date: Vec<ExpenseByDate>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExpenseByType {
    pub expense_type: String,
    pub total_amount: f64,
    pub count: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExpenseByCompany {
    pub company: String,
    pub total_amount: f64,
    pub count: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExpenseByCar {
    pub car_no_plate: String,
    pub total_amount: f64,
    pub count: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExpenseByPaymentMethod {
    pub payment_method: String,
    pub total_amount: f64,
    pub count: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExpenseByDate {
    pub date: String,
    pub total_amount: f64,
    pub count: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExpenseStatisticsResponse {
    pub message: String,
    pub data: ExpenseStatistics,
}

// ============================================================================
// Batch Create Models
// ============================================================================

#[derive(Debug, Serialize)]
pub struct BatchCreateResult {
    pub success_count: usize,
    pub failed_count: usize,
    pub created_expenses: Vec<FleetExpense>,
    pub errors: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct FleetExpenseBatchResponse {
    pub message: String,
    pub data: BatchCreateResult,
}