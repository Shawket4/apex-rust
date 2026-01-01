use serde::{Deserialize, Serialize};
use chrono::{NaiveDate, NaiveDateTime};

// ============================================================================
// Expense Source Enum - identifies where the expense came from
// ============================================================================

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ExpenseSource {
    FleetExpense,
    FuelEvent,
    Loan,
}

// ============================================================================
// Unified Expense Model - represents expenses from all sources
// ============================================================================

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UnifiedExpense {
    pub id: i32,
    pub source: ExpenseSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub car_no_plate: Option<String>,
    pub expense_date: NaiveDate,
    pub expense_type: String,
    pub amount: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub company: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paid_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payment_method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<i32>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    
    // Fuel-specific fields (optional, only populated for fuel events)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub liters: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_per_liter: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub odometer_before: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub odometer_after: Option<i64>,
    
    // Loan-specific fields (optional, only populated for loans)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_paid: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub employee_id: Option<i64>,
}

// ============================================================================
// Original Fleet Expense Model (keep for CRUD operations)
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
    pub source: Option<String>,
    // Use String to handle "true"/"false" from query params
    pub include_fuel: Option<String>,
    pub include_loans: Option<String>,
}

impl FleetExpenseFilters {
    pub fn should_include_fuel(&self) -> bool {
        self.include_fuel.as_ref().map(|s| s != "false").unwrap_or(true)
    }
    
    pub fn should_include_loans(&self) -> bool {
        self.include_loans.as_ref().map(|s| s != "false").unwrap_or(true)
    }
}

// ============================================================================
// Response Models
// ============================================================================

#[derive(Debug, Serialize, Deserialize)]
pub struct UnifiedExpenseListResponse {
    pub message: String,
    pub data: Vec<UnifiedExpense>,
    pub pagination: PaginationInfo,
    pub source_counts: SourceCounts,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SourceCounts {
    pub fleet_expenses: i64,
    pub fuel_events: i64,
    pub loans: i64,
    pub total: i64,
}

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
// Statistics Models (Updated to include source breakdown)
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
    pub by_source: Vec<ExpenseBySource>,
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
pub struct ExpenseBySource {
    pub source: String,
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