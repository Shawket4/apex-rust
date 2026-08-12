-- Reverses 20260808120000_banksms_schema.up.sql.
--
-- Drops the whole `banksms` schema. Nothing in `public` is touched, so this is
-- safe to run against production: it cannot affect FalconGo, apex-petroapp, or
-- the existing apex-rust endpoints.
--
-- pg_trgm is deliberately NOT dropped. It is database-global and another schema
-- may have come to depend on it; dropping an extension we merely ensured exists
-- would be a side effect beyond this migration's scope.
--
-- The `banksms` schema itself is also NOT dropped: sqlx's own bookkeeping table
-- lives in it as `banksms._sqlx_migrations` (see run_banksms_migrations in
-- main.rs for why it is there rather than in `public`). Dropping the schema would
-- delete the table recording this very revert. An empty schema is harmless.

DROP TABLE IF EXISTS banksms.noise_skeletons;
DROP TABLE IF EXISTS banksms.parse_templates;
DROP TABLE IF EXISTS banksms.ingest_cursor;
DROP TABLE IF EXISTS banksms.transaction_tags;
DROP TABLE IF EXISTS banksms.tags;
DROP TABLE IF EXISTS banksms.notes;
DROP TABLE IF EXISTS banksms.transaction_overrides;
DROP TABLE IF EXISTS banksms.transactions;
DROP TABLE IF EXISTS banksms.raw_messages;

DROP FUNCTION IF EXISTS banksms.touch_updated_at();

DROP TYPE IF EXISTS banksms.direction;
DROP TYPE IF EXISTS banksms.parse_method;
DROP TYPE IF EXISTS banksms.txn_source;
DROP TYPE IF EXISTS banksms.parse_status;

-- Intentionally no `DROP SCHEMA banksms` here -- see the note at the top.
