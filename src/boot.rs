//! Boot-time database wiring, shared by the binary and the integration suite.

use log::info;

/// Is the legacy banksms schema still in place (i.e. `apex-rust cutover` has
/// not run)? Detected by a table only the legacy schema has.
pub async fn legacy_schema_present(pool: &sqlx::PgPool) -> bool {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT to_regclass('banksms.transaction_overrides')::text",
    )
    .fetch_one(pool)
    .await
    .ok()
    .flatten()
    .is_some()
}

/// Apply the banksms migrations, bookkeeping isolated in
/// `banksms._sqlx_migrations` via a search_path-scoped connection —
/// apex-petroapp owns `public._sqlx_migrations` in this same database and the
/// two must never share a table. Post-cutover this is a no-op (the cutover
/// stamps the baseline); on a fresh database it creates everything.
pub async fn run_banksms_migrations(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    use sqlx::Executor;

    sqlx::query("CREATE SCHEMA IF NOT EXISTS banksms")
        .execute(pool)
        .await?;

    let mut conn = pool.acquire().await?;
    conn.execute("SET search_path TO banksms, public").await?;

    sqlx::migrate!("./migrations")
        .run(&mut *conn)
        .await
        .map_err(|e| sqlx::Error::Migrate(Box::new(e)))?;

    info!("Migrations applied (banksms._sqlx_migrations)");
    Ok(())
}
