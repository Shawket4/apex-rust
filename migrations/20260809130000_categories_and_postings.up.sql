-- Categories with per-category requirements, party links, and the posting ledger.
--
-- The goal: categorising a transaction as an advance should register a real row
-- in public.loans for that driver or employee, so it reaches payroll -- without
-- turning banksms into a second uncontrolled writer of FalconGo's tables.
--
-- The invariant that makes this safe:
--
--     apex-rust only ever modifies `loans` rows it created itself,
--     and banksms.transaction_postings records exactly which those are.
--
-- Everything here exists to keep that invariant true and checkable.

-- ---------------------------------------------------------------------------
-- What a category may require, and what it posts to.
-- ---------------------------------------------------------------------------

-- Which downstream table a category materialises into. `none` is the common
-- case: most categories are just labels.
CREATE TYPE banksms.posting_target AS ENUM ('none', 'loan');

-- Who the transaction is about. Drivers and employees are separate tables in
-- FalconGo, and `loans` carries a nullable FK to each.
CREATE TYPE banksms.party_kind AS ENUM ('none', 'driver', 'employee', 'either');

CREATE TABLE banksms.categories (
    id              BIGSERIAL PRIMARY KEY,
    -- Stable machine key. `transactions.category` stores this string, so the
    -- 8 values migrated out of fleet_expenses keep working untouched.
    key             TEXT NOT NULL,
    label           TEXT NOT NULL,
    label_ar        TEXT,

    posting_target  banksms.posting_target NOT NULL DEFAULT 'none',
    required_party  banksms.party_kind     NOT NULL DEFAULT 'none',

    -- Extra per-category fields, rendered by the form and validated by the API.
    -- Data rather than code: adding a category must not require a deploy, and a
    -- growing `match` over category names in two languages is exactly what this
    -- avoids.
    field_spec      JSONB NOT NULL DEFAULT '[]'::jsonb,

    enabled         BOOLEAN NOT NULL DEFAULT true,
    sort_order      INTEGER NOT NULL DEFAULT 100,
    created_by      TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT categories_key_not_blank CHECK (length(btrim(key)) > 0),
    CONSTRAINT categories_field_spec_is_array CHECK (jsonb_typeof(field_spec) = 'array'),

    -- A category that posts to `loans` MUST identify a party -- a loan with
    -- neither driver_id nor employee_id is unattributable money.
    CONSTRAINT categories_posting_needs_party CHECK (
        posting_target = 'none' OR required_party <> 'none'
    )
);

CREATE UNIQUE INDEX categories_key_idx ON banksms.categories (lower(key));

CREATE TRIGGER categories_touch
    BEFORE UPDATE ON banksms.categories
    FOR EACH ROW EXECUTE FUNCTION banksms.touch_updated_at();

-- ---------------------------------------------------------------------------
-- Party reference on the transaction.
--
-- Two nullable FKs rather than a free-text name. `loans.method` in this same
-- database is the cautionary tale: one person is stored there as 'Shady',
-- 'shady', 'شادى', 'Shady (تاني)' and 'Shady (x2)', and the column also
-- accumulated payment methods, reasons, dates and whole sentences. Ids cannot
-- decay that way.
--
-- Real foreign keys, so a party cannot be invented by a typo. Both FalconGo
-- tables use GORM soft deletes, so these constraints never block its deletes.
-- ---------------------------------------------------------------------------

ALTER TABLE banksms.transactions
    ADD COLUMN driver_id   BIGINT REFERENCES public.drivers(id),
    ADD COLUMN employee_id BIGINT REFERENCES public.employees(id),
    -- Free-form per-category values described by categories.field_spec.
    ADD COLUMN attributes  JSONB NOT NULL DEFAULT '{}'::jsonb,

    -- A transaction is about at most one person. Both set is a bug, and it
    -- would make the posted loan ambiguous.
    ADD CONSTRAINT transactions_single_party CHECK (
        driver_id IS NULL OR employee_id IS NULL
    ),
    ADD CONSTRAINT transactions_attributes_is_object CHECK (
        jsonb_typeof(attributes) = 'object'
    );

CREATE INDEX transactions_driver_id_idx
    ON banksms.transactions (driver_id) WHERE driver_id IS NOT NULL;
CREATE INDEX transactions_employee_id_idx
    ON banksms.transactions (employee_id) WHERE employee_id IS NOT NULL;

-- ---------------------------------------------------------------------------
-- The posting ledger -- the load-bearing table.
--
-- Without it there is no way to answer "which loans came from a bank SMS?", and
-- no safe way to undo a miscategorisation. With it, the side effect is
-- explicit, idempotent and reversible, which is the whole difference between a
-- hidden coupling and an ordinary relationship.
-- ---------------------------------------------------------------------------

CREATE TABLE banksms.transaction_postings (
    transaction_id  BIGINT PRIMARY KEY
                        REFERENCES banksms.transactions(id) ON DELETE CASCADE,
    target_table    TEXT   NOT NULL,
    target_id       BIGINT NOT NULL,
    posted_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    posted_by       TEXT,

    CONSTRAINT postings_target_known CHECK (target_table IN ('loans'))
);

-- One posted row per target. Belt and braces against double-posting: even if
-- application logic were wrong, the database refuses to create a second loan
-- pointing at the same row.
CREATE UNIQUE INDEX transaction_postings_target_idx
    ON banksms.transaction_postings (target_table, target_id);

-- ---------------------------------------------------------------------------
-- Seed.
--
-- The 8 keys already present in migrated fleet_expenses data, so nothing
-- becomes uncategorised, plus the advance/loan categories that motivated this.
-- Only the advance categories post; the rest are plain labels.
-- ---------------------------------------------------------------------------

INSERT INTO banksms.categories
    (key, label, label_ar, posting_target, required_party, sort_order, created_by)
VALUES
    ('Advance',      'Advance / Loan',   'سلفة',        'loan', 'either', 10, 'migration'),
    ('Salary',       'Salary payment',   'راتب',        'none', 'either', 20, 'migration'),
    ('Fuel',         'Fuel',             'وقود',        'none', 'none',   30, 'migration'),
    ('Labor',        'Labor',            'عمالة',       'none', 'none',   40, 'migration'),
    ('Parts',        'Parts',            'قطع غيار',    'none', 'none',   50, 'migration'),
    ('Maintenance',  'Maintenance',      'صيانة',       'none', 'none',   60, 'migration'),
    ('Repairs',      'Repairs',          'إصلاحات',     'none', 'none',   70, 'migration'),
    ('Insurance',    'Insurance',        'تأمين',       'none', 'none',   80, 'migration'),
    ('Registration', 'Registration',     'ترخيص',       'none', 'none',   90, 'migration'),
    ('Other',        'Other',            'أخرى',        'none', 'none',  999, 'migration')
ON CONFLICT DO NOTHING;
