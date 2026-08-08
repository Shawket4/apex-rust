-- Reverses 20260808120200_migrate_fleet_expenses.up.sql.
--
-- Removes only rows that came from public.fleet_expenses, identified by
-- import_source_id. Any other imported row (a future CSV import, say) is left
-- alone, and manual/whatsapp rows are never in scope.
--
-- Dependent overrides, notes and tags go with them via ON DELETE CASCADE, which
-- is correct: they annotate a row that is about to stop existing.

DELETE FROM banksms.transactions
WHERE source = 'import'
  AND import_source_id IN (SELECT id FROM public.fleet_expenses);
