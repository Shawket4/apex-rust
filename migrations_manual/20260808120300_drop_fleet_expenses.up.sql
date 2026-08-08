-- Drop public.fleet_expenses, now that its data lives in banksms.transactions.
--
-- This is the deliberate exception to "do not modify existing tables", approved
-- explicitly. It is a SEPARATE migration from the copy (20260808120200) so the
-- data can be verified in place before the source disappears.
--
-- Scope check, done during recon:
--   * public.fleet_expenses has NO Go model and is NOT in FalconGo's GORM
--     AutoMigrate list, so dropping it is permanent.
--   * public.expenses and public.loans are DIFFERENT tables that ARE
--     GORM-AutoMigrated by FalconGo (Models/setup.go) and back four live
--     FalconGo routes plus payslip calculations. They are deliberately NOT
--     touched here: dropping either would break FalconGo, and GORM would
--     recreate it empty on the next restart anyway.
--
-- The table is renamed rather than dropped outright. `apex-rust` still serves
-- /fleet-expenses as a union over fleet_expenses + fuel_events + loans, so the
-- rename makes any missed reference fail loudly and immediately instead of
-- silently returning an empty result set. Drop the archived table by hand once
-- the dashboard has been moved across.

DO $$
DECLARE
    src_count BIGINT;
    dst_count BIGINT;
    src_total NUMERIC;
    dst_total NUMERIC;
BEGIN
    IF to_regclass('public.fleet_expenses') IS NULL THEN
        RAISE NOTICE 'public.fleet_expenses already gone; nothing to drop';
        RETURN;
    END IF;

    -- Refuse to drop anything that has not been fully copied. A migration that
    -- destroys the only copy of 223 rows because an earlier step silently no-oped
    -- is not a recoverable mistake.
    SELECT count(*), coalesce(sum(amount), 0) INTO src_count, src_total
    FROM public.fleet_expenses;

    SELECT count(*), coalesce(sum(parsed_amount), 0) INTO dst_count, dst_total
    FROM banksms.transactions
    WHERE source = 'import'
      AND import_source_id IN (SELECT id FROM public.fleet_expenses);

    IF src_count <> dst_count OR src_total <> dst_total THEN
        RAISE EXCEPTION
            'refusing to drop fleet_expenses: source has % rows / %, migrated has % rows / %',
            src_count, src_total, dst_count, dst_total;
    END IF;

    ALTER TABLE public.fleet_expenses RENAME TO fleet_expenses_archived_20260808;

    RAISE NOTICE
        'fleet_expenses archived as fleet_expenses_archived_20260808 (% rows, total % verified in banksms.transactions)',
        src_count, src_total;
END $$;
