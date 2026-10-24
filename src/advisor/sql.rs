//! The model's read-only window onto the `apex` database.
//!
//! Three layers, because the statement text comes from a model:
//! 1. shape — one SELECT/WITH statement, no write keywords, nothing that names
//!    a secret column or reaches outside the data (`pg_*` functions read files
//!    and settings when the role is privileged);
//! 2. a READ ONLY transaction that is always rolled back;
//! 3. a statement timeout and a row cap, so a cartesian scan cannot hold a
//!    connection FalconGo needs.

use serde_json::{json, Value};
use sqlx::PgPool;

/// Rows handed back to the model. Enough to reason over, small enough that a
/// careless `SELECT *` cannot blow the context window.
pub const MAX_ROWS: i64 = 200;
const STATEMENT_TIMEOUT: &str = "8s";

/// Why a statement was refused, worded for the model so it can correct itself.
pub fn check(sql: &str) -> Result<(), String> {
    let lowered = sql.to_lowercase();
    let t = lowered.trim_start();
    if !(t.starts_with("select") || t.starts_with("with")) {
        return Err("only a single SELECT (or WITH … SELECT) statement is allowed".into());
    }
    let body = t.trim_end().trim_end_matches(';');
    if body.contains(';') {
        return Err("one statement only — remove the extra ';'".into());
    }
    let words: Vec<&str> = body
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .collect();
    const WRITES: &[&str] = &[
        "insert", "update", "delete", "drop", "alter", "truncate", "create", "grant", "revoke",
        "copy", "merge", "call", "do", "vacuum", "analyze", "listen", "notify", "lock", "into",
        "set", "reset", "execute", "prepare", "refresh",
    ];
    if let Some(w) = words.iter().find(|w| WRITES.contains(w)) {
        return Err(format!("`{w}` is not allowed: this tool only reads"));
    }
    // Secrets: users.password, driver PINs, tokens. The advisor never needs them.
    const SECRET: &[&str] = &["password", "pin", "token", "secret", "api_key", "otp", "jwt"];
    if let Some(w) = words.iter().find(|w| SECRET.contains(w)) {
        return Err(format!("`{w}` is off limits"));
    }
    if let Some(w) = words.iter().find(|w| {
        w.starts_with("pg_")
            || w.starts_with("lo_")
            || w.starts_with("dblink")
            || **w == "current_setting"
            || **w == "set_config"
            || **w == "information_schema"
    }) {
        return Err(format!("`{w}` is not available; the schema is in your instructions"));
    }
    Ok(())
}

/// Run a model-written query. Errors come back as `{"error": …}` values rather
/// than `Err`, because the model reads them and retries.
pub async fn run(pool: &PgPool, sql: &str) -> Value {
    if let Err(why) = check(sql) {
        return json!({ "error": why });
    }
    let body = sql.trim().trim_end_matches(';');
    let inner = async {
        let mut tx = pool.begin().await?;
        sqlx::query("SET TRANSACTION READ ONLY")
            .execute(&mut *tx)
            .await?;
        sqlx::query(&format!("SET LOCAL statement_timeout = '{STATEMENT_TIMEOUT}'"))
            .execute(&mut *tx)
            .await?;
        // One extra row tells us whether the cap cut anything off.
        let (rows,): (Value,) = sqlx::query_as(&format!(
            "SELECT COALESCE(jsonb_agg(t), '[]'::jsonb) FROM (SELECT * FROM ({body}) q LIMIT {}) t",
            MAX_ROWS + 1
        ))
        .fetch_one(&mut *tx)
        .await?;
        tx.rollback().await?;
        Ok::<Value, sqlx::Error>(rows)
    };
    match inner.await {
        Ok(Value::Array(mut rows)) => {
            let truncated = rows.len() as i64 > MAX_ROWS;
            rows.truncate(MAX_ROWS as usize);
            json!({ "row_count": rows.len(), "truncated": truncated, "rows": rows })
        }
        Ok(other) => json!({ "rows": other }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

#[cfg(test)]
mod tests {
    use super::check;

    #[test]
    fn plain_reads_pass() {
        assert!(check("SELECT id, car_no_plate FROM cars WHERE deleted_at IS NULL").is_ok());
        assert!(check("with t as (select 1) select * from t;").is_ok());
    }

    #[test]
    fn writes_and_stacked_statements_are_refused() {
        assert!(check("UPDATE cars SET operating_company = 'x'").is_err());
        assert!(check("select 1; delete from cars").is_err());
        assert!(check("select * into x from cars").is_err());
        assert!(check("with d as (delete from cars returning *) select * from d").is_err());
    }

    #[test]
    fn secrets_and_server_functions_are_refused() {
        assert!(check("select email, password from users").is_err());
        assert!(check("select pg_read_file('/etc/passwd')").is_err());
        assert!(check("select current_setting('data_directory')").is_err());
        assert!(check("select * from information_schema.columns").is_err());
    }

    #[test]
    fn words_inside_identifiers_do_not_trip_the_filter() {
        // `updated_at`, `created_at`, `drop_off_point` contain banned words
        // only as substrings of a larger identifier.
        assert!(check("select updated_at, created_at, drop_off_point from trips").is_ok());
    }
}
