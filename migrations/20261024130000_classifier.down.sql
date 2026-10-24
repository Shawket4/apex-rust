DROP TABLE IF EXISTS banksms.classifications;
DROP TABLE IF EXISTS banksms.payee_aliases;
DROP TABLE IF EXISTS banksms.payees;
ALTER TABLE banksms.transactions DROP COLUMN IF EXISTS linked_loan_id;
-- Categories in use by transactions stay; the unused ones go.
DELETE FROM banksms.categories c
WHERE c.key IN ('DriverCustody', 'Tires', 'Utilities', 'GovFees', 'BankFees', 'CashWithdrawal',
                'TrucksEquipment', 'BankLoan', 'InternalTransfer', 'PetroApp', 'Reversed', 'Personal')
  AND NOT EXISTS (SELECT 1 FROM banksms.transactions t WHERE t.category = c.key);
ALTER TABLE banksms.categories DROP COLUMN IF EXISTS report_group;
