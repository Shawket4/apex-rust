-- Reverses 20260809160000_loan_kinds.up.sql.
--
-- loans.kind is deliberately NOT dropped. FalconGo's Loan model declares the
-- column, so removing it would break that service the moment it next reads a
-- loan, and the classification it holds is information that took a decision to
-- produce. Reverting the schema should not destroy it.

DELETE FROM banksms.categories
WHERE key IN ('Loan', 'SalaryPortion') AND created_by = 'migration';

UPDATE banksms.categories SET label = 'Advance / Loan' WHERE key = 'Advance';

ALTER TABLE banksms.categories DROP COLUMN IF EXISTS posting_kind;

ALTER TABLE banksms.transactions DROP COLUMN IF EXISTS car_id;
