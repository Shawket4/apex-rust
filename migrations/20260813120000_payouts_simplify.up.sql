-- Payroll is deprecated, and money handed out ahead of a salary is recorded
-- as an advance or a loan — which is what actually registers in the payouts
-- ledger. 'Part of salary' therefore leaves the picker (zero rows ever used
-- it, verified in production 2026-08-13).
--
-- 'Salary payment' stays as a pure label: the person becomes free text on the
-- transaction (paid_by), not an id link, and nothing registers anywhere.
DELETE FROM banksms.categories WHERE key = 'SalaryPortion';
UPDATE banksms.categories SET required_party = 'none' WHERE key = 'Salary';
