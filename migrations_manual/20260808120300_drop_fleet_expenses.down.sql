-- Reverses 20260808120300_drop_fleet_expenses.up.sql by restoring the name.
--
-- Only possible because the up migration ARCHIVED the table rather than dropping
-- it. Once the archived table is dropped by hand, this revert can no longer
-- restore anything -- restore from a dump at that point.

DO $$
BEGIN
    IF to_regclass('public.fleet_expenses_archived_20260808') IS NULL THEN
        RAISE NOTICE 'no archived fleet_expenses table to restore';
        RETURN;
    END IF;

    IF to_regclass('public.fleet_expenses') IS NOT NULL THEN
        RAISE EXCEPTION
            'public.fleet_expenses already exists; refusing to overwrite it';
    END IF;

    ALTER TABLE public.fleet_expenses_archived_20260808 RENAME TO fleet_expenses;
    RAISE NOTICE 'fleet_expenses restored';
END $$;
