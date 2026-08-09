-- Reverses 20260809130000_categories_and_postings.up.sql.
--
-- Deliberately does NOT delete the public.loans rows this feature posted.
-- Reverting a schema migration must not silently remove money from payroll --
-- those rows are correlated by banksms.transaction_postings, and dropping that
-- table loses the correlation, so un-posting has to be an explicit, reviewed
-- action rather than a side effect of a rollback.

DROP TABLE IF EXISTS banksms.transaction_postings;

ALTER TABLE banksms.transactions
    DROP CONSTRAINT IF EXISTS transactions_attributes_is_object,
    DROP CONSTRAINT IF EXISTS transactions_single_party,
    DROP COLUMN IF EXISTS attributes,
    DROP COLUMN IF EXISTS employee_id,
    DROP COLUMN IF EXISTS driver_id;

DROP TABLE IF EXISTS banksms.categories;

DROP TYPE IF EXISTS banksms.party_kind;
DROP TYPE IF EXISTS banksms.posting_target;
