-- Reverses 20260809190000_drop_verified.up.sql.
--
-- Restored as `true` rather than `false`: every row that existed when the column
-- was dropped had already been accepted into the ledger, and reintroducing them
-- as unverified would misrepresent them as awaiting review.

ALTER TABLE banksms.transactions
    ADD COLUMN IF NOT EXISTS verified BOOLEAN NOT NULL DEFAULT true;
