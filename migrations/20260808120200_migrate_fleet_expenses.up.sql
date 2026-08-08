-- Migrate public.fleet_expenses into banksms.transactions.
--
-- 223 rows as of 2026-08-08, expense_date 2025-11-01 .. 2026-02-19.
-- The source table is NOT dropped here -- that is a separate, later migration,
-- run only once this data has been verified in place. Copy first, verify, then
-- drop; never in one step.
--
-- Idempotent: keyed on import_source_id via a partial unique index, so re-running
-- is a no-op rather than a double-count.
--
-- Timezone note: fleet_expenses.created_at/updated_at are `timestamp without
-- time zone` written by CURRENT_TIMESTAMP on a server whose TimeZone is Etc/UTC,
-- so they are UTC wall-clock and convert with `AT TIME ZONE 'UTC'`.
-- expense_date is a plain business date and becomes midnight Africa/Cairo --
-- using UTC there would shift ~2h of rows onto the previous day in reports.

INSERT INTO banksms.transactions (
    source,
    import_source_id,
    parsed_direction,
    parsed_amount,
    parsed_currency,
    parsed_occurred_at,
    parse_method,
    parser_version,
    confidence,
    description,
    payment_method,
    company,
    car_no_plate,
    paid_by,
    category,
    verified,
    created_by,
    created_at,
    updated_at,
    deleted_at
)
SELECT
    'import'::banksms.txn_source,
    fe.id,

    -- Every fleet expense is money leaving. The table has no direction column
    -- and no negative amounts (CHECK amount >= 0), so this is not a guess.
    'out'::banksms.direction,

    fe.amount,
    'EGP',

    -- Business date -> midnight in Cairo.
    (fe.expense_date::timestamp AT TIME ZONE 'Africa/Cairo'),

    -- Hand-entered, not machine-parsed. parser_version 0 marks "no parser ran",
    -- which keeps these rows outside every re-parse scope (which is additionally
    -- guarded on source = 'whatsapp').
    'manual'::banksms.parse_method,
    0,
    100,

    fe.description,
    fe.payment_method,
    fe.company,
    fe.car_no_plate,
    fe.paid_by,

    -- The "categories" carried over verbatim: Other, Labor, Parts, Registration,
    -- Fuel, Maintenance, Repairs, Insurance.
    fe.expense_type,

    -- A human deliberately entered each of these and they were the system of
    -- record, so they arrive already verified rather than in a review queue.
    true,

    fe.created_by::text,

    (fe.created_at AT TIME ZONE 'UTC'),
    (fe.updated_at AT TIME ZONE 'UTC'),
    (fe.deleted_at AT TIME ZONE 'UTC')
FROM public.fleet_expenses fe
ON CONFLICT (import_source_id) WHERE import_source_id IS NOT NULL DO NOTHING;

-- Fail the migration loudly if anything was silently dropped or the money moved.
DO $$
DECLARE
    src_count   BIGINT;
    dst_count   BIGINT;
    src_total   NUMERIC;
    dst_total   NUMERIC;
BEGIN
    SELECT count(*), coalesce(sum(amount), 0) INTO src_count, src_total
    FROM public.fleet_expenses;

    SELECT count(*), coalesce(sum(parsed_amount), 0) INTO dst_count, dst_total
    FROM banksms.transactions WHERE source = 'import';

    IF src_count <> dst_count THEN
        RAISE EXCEPTION 'fleet_expenses migration row mismatch: source %, migrated %',
            src_count, dst_count;
    END IF;

    IF src_total <> dst_total THEN
        RAISE EXCEPTION 'fleet_expenses migration total mismatch: source %, migrated %',
            src_total, dst_total;
    END IF;

    RAISE NOTICE 'fleet_expenses migrated: % rows, total %', dst_count, dst_total;
END $$;
