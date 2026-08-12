-- Reverses 20260809170000_review_terminal_and_drop_notes.up.sql.
--
-- The notes table is recreated empty. Its contents are not recoverable, which
-- is the honest consequence of dropping it; the down migration exists so the
-- schema can be restored, not the data.

CREATE TABLE IF NOT EXISTS banksms.notes (
    id              BIGSERIAL PRIMARY KEY,
    transaction_id  BIGINT NOT NULL REFERENCES banksms.transactions(id) ON DELETE CASCADE,
    body            TEXT NOT NULL,
    author          TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at      TIMESTAMPTZ
);

ALTER TABLE banksms.raw_messages
    DROP COLUMN IF EXISTS reviewed_by,
    DROP COLUMN IF EXISTS reviewed_at;
