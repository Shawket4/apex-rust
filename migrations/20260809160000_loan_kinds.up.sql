-- Loan kinds: advance / loan / salary.
--
-- All three subtract identically. What differs is what they MEAN, and until now
-- the table could not say: everything was a "loan" regardless of whether it was
-- money paid ahead of earnings, a genuine debt, or part of the salary itself.
--
-- The column is created HERE rather than left to FalconGo's GORM AutoMigrate,
-- so this does not depend on which service boots first. AutoMigrate sees an
-- existing column and leaves it alone; the Go model declares the same type and
-- default, so the two agree.

DO $$
BEGIN
    IF to_regclass('public.loans') IS NULL THEN
        RAISE NOTICE 'public.loans does not exist; nothing to do';
        RETURN;
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_schema = 'public' AND table_name = 'loans' AND column_name = 'kind'
    ) THEN
        -- NOT NULL with a default backfills every existing row in one pass.
        ALTER TABLE public.loans
            ADD COLUMN kind VARCHAR(16) NOT NULL DEFAULT 'advance';
        RAISE NOTICE 'added loans.kind';
    END IF;

    -- Every row that predates this column is an advance: money paid out ahead of
    -- earnings. There were no genuine loans in the data -- confirmed with the
    -- owner -- so classifying them as advances is a restatement of what they
    -- always were, not a guess.
    UPDATE public.loans
    SET kind = 'advance'
    WHERE kind IS NULL OR btrim(kind) = '';
END $$;

CREATE INDEX IF NOT EXISTS idx_loans_kind ON public.loans (kind);

-- Reject anything outside the three known kinds. A typo here would silently
-- create a fourth category that no report knows to include.
DO $$
BEGIN
    IF to_regclass('public.loans') IS NOT NULL
       AND NOT EXISTS (
           SELECT 1 FROM pg_constraint WHERE conname = 'loans_kind_known'
       )
    THEN
        ALTER TABLE public.loans
            ADD CONSTRAINT loans_kind_known
            CHECK (kind IN ('advance', 'loan', 'salary'));
    END IF;
END $$;

-- ---------------------------------------------------------------------------
-- Categories that post each kind.
--
-- 'Advance' already exists and keeps its key so migrated transactions stay
-- categorised. The other two are new.
-- ---------------------------------------------------------------------------

ALTER TABLE banksms.categories
    ADD COLUMN IF NOT EXISTS posting_kind TEXT;

-- Which loan kind a posting category produces. Null for categories that post
-- nothing, which is why this is a plain column rather than part of the enum.
UPDATE banksms.categories SET posting_kind = 'advance'
WHERE key = 'Advance' AND posting_kind IS NULL;

UPDATE banksms.categories
SET label = 'Advance', label_ar = 'سلفة'
WHERE key = 'Advance';

INSERT INTO banksms.categories
    (key, label, label_ar, posting_target, required_party, posting_kind, sort_order, created_by)
VALUES
    ('Loan', 'Loan', 'قرض', 'loan', 'either', 'loan', 11, 'migration'),
    ('SalaryPortion', 'Part of salary', 'جزء من الراتب', 'loan', 'either', 'salary', 12, 'migration')
ON CONFLICT DO NOTHING;

-- ---------------------------------------------------------------------------
-- Vehicle by id.
--
-- car_no_plate stays as-is: it holds the plate exactly as the legacy data
-- recorded it, and rewriting 223 imported rows to guess at ids would be a
-- lossy migration of data nobody asked to change. New and edited rows carry
-- car_id, and the plate is derived from it for display.
-- ---------------------------------------------------------------------------

ALTER TABLE banksms.transactions
    ADD COLUMN IF NOT EXISTS car_id BIGINT REFERENCES public.cars(id);

CREATE INDEX IF NOT EXISTS transactions_car_id_idx
    ON banksms.transactions (car_id) WHERE car_id IS NOT NULL;

-- Backfill where the recorded plate matches exactly one vehicle. Ambiguous or
-- unmatched plates are left alone rather than guessed at.
UPDATE banksms.transactions t
SET car_id = c.id
FROM public.cars c
WHERE t.car_id IS NULL
  AND t.car_no_plate IS NOT NULL
  AND btrim(t.car_no_plate) <> ''
  AND c.deleted_at IS NULL
  AND btrim(c.car_no_plate) = btrim(t.car_no_plate)
  AND (SELECT count(*) FROM public.cars c2
       WHERE c2.deleted_at IS NULL AND btrim(c2.car_no_plate) = btrim(t.car_no_plate)) = 1;
