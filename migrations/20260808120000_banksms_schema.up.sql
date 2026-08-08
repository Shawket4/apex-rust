-- Bank-SMS transaction ingestion: schema, enums, tables, indexes.
--
-- Everything lives in the dedicated `banksms` schema. Additive only: no existing
-- table in `public` is touched by this migration.
--
-- Note on the migration bookkeeping table: `public._sqlx_migrations` is owned by
-- apex-petroapp, which applies its own versions 1..3 to this same database.
-- Sharing it would make each migrator reject the other's rows as unknown
-- versions, so this service's migrator runs with `search_path` starting at
-- `banksms` and its bookkeeping lands in `banksms._sqlx_migrations`.
-- See `run_banksms_migrations` in src/main.rs.
--
-- Consequence for the down migration: it must NOT drop the `banksms` schema
-- itself, only its contents.

CREATE SCHEMA IF NOT EXISTS banksms;

-- Required for the trigram index on parsed_counterparty. pg_trgm is a "trusted"
-- extension in PG13+, so the database owner (`apex`) can install it without
-- superuser. Installed into `public` because extensions are database-global.
CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- ---------------------------------------------------------------------------
-- Enums
-- ---------------------------------------------------------------------------

-- Every raw message ends in exactly one terminal status. `pending` is the only
-- non-terminal value; `ignored` means the triage gate rejected it (expected,
-- not an error); `error` means the parser itself blew up.
CREATE TYPE banksms.parse_status AS ENUM (
    'pending', 'parsed', 'partial', 'unmatched', 'ignored', 'error'
);

CREATE TYPE banksms.txn_source AS ENUM ('whatsapp', 'manual', 'import');

CREATE TYPE banksms.parse_method AS ENUM ('template', 'extractors', 'manual');

CREATE TYPE banksms.direction AS ENUM ('in', 'out');

-- ---------------------------------------------------------------------------
-- Shared updated_at trigger
-- ---------------------------------------------------------------------------

CREATE FUNCTION banksms.touch_updated_at() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    NEW.updated_at := now();
    RETURN NEW;
END;
$$;

-- ---------------------------------------------------------------------------
-- raw_messages — every message we ever saw, verbatim. Never mutated except for
-- parse bookkeeping. Store raw first, parse second.
-- ---------------------------------------------------------------------------

CREATE TABLE banksms.raw_messages (
    id              BIGSERIAL PRIMARY KEY,

    -- The dedup key. Non-negotiable: the poller relies on ON CONFLICT here to
    -- make its overlapping re-polls free.
    wa_message_id   TEXT        NOT NULL UNIQUE,

    chat_jid        TEXT        NOT NULL,
    sender          TEXT,
    is_from_me      BOOLEAN     NOT NULL DEFAULT false,

    -- The WhatsApp envelope timestamp, already UTC from the API.
    wa_timestamp    TIMESTAMPTZ NOT NULL,

    -- Verbatim message body. Empty string for media messages; never normalized.
    -- Normalization happens on a copy, at parse time.
    body            TEXT        NOT NULL,

    ingested_at     TIMESTAMPTZ NOT NULL DEFAULT now(),

    parse_status    banksms.parse_status NOT NULL DEFAULT 'pending',
    parser_version  INTEGER,

    -- Template fingerprint: digits/names/refs replaced with placeholders, hashed.
    -- Stored for EVERY message including `ignored` ones, because recurrence count
    -- is what separates "a bank changed its format" from "a human message".
    skeleton_hash   TEXT,

    -- How bank-SMS-like the triage gate judged this message (0-100).
    triage_score    SMALLINT,

    parsed_at       TIMESTAMPTZ,
    parse_error     TEXT,

    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT raw_messages_triage_score_range
        CHECK (triage_score IS NULL OR triage_score BETWEEN 0 AND 100)
);

CREATE TRIGGER raw_messages_touch
    BEFORE UPDATE ON banksms.raw_messages
    FOR EACH ROW EXECUTE FUNCTION banksms.touch_updated_at();

-- The review queue orders by skeleton frequency, so this one is load-bearing.
CREATE INDEX raw_messages_skeleton_hash_idx
    ON banksms.raw_messages (skeleton_hash);

CREATE INDEX raw_messages_parse_status_idx
    ON banksms.raw_messages (parse_status);

-- Cursor advance and backfill both walk this.
CREATE INDEX raw_messages_chat_ts_idx
    ON banksms.raw_messages (chat_jid, wa_timestamp DESC);

-- ---------------------------------------------------------------------------
-- transactions
--
-- Column ownership is the load-bearing idea here:
--   * parsed_*      written by whatever produced the row (parser / importer /
--                   manual create), then treated as immutable source-of-record.
--                   The re-parse job rebuilds these ONLY where source='whatsapp'.
--   * category, verified, deleted_at   user-owned, freely editable.
--   * corrections to parsed_* go into banksms.transaction_overrides, never here.
--     Effective value = COALESCE(override, parsed_*).
-- ---------------------------------------------------------------------------

CREATE TABLE banksms.transactions (
    id                  BIGSERIAL PRIMARY KEY,

    source              banksms.txn_source NOT NULL,

    -- NULL for manual and imported rows.
    raw_message_id      BIGINT REFERENCES banksms.raw_messages(id),

    -- Provenance for migrated rows: the original public.fleet_expenses.id.
    -- Makes the data migration idempotent and re-runnable.
    import_source_id    BIGINT,

    -- Optimistic concurrency. Bumped by the API on every mutation; clients send
    -- If-Match and get 409 on mismatch.
    version             INTEGER NOT NULL DEFAULT 1,

    -- --- parser-owned ------------------------------------------------------
    parsed_direction    banksms.direction,
    parsed_amount       NUMERIC(18,4),
    parsed_currency     TEXT,
    parsed_account      TEXT,
    parsed_counterparty TEXT,
    parsed_reference    TEXT,
    parsed_balance_after NUMERIC(18,4),
    parsed_occurred_at  TIMESTAMPTZ,
    parsed_template     TEXT,
    parser_version      INTEGER,

    confidence          SMALLINT NOT NULL DEFAULT 100,
    parse_method        banksms.parse_method,

    -- --- carried over from public.fleet_expenses ---------------------------
    -- The existing costs view filters on all four of these, so they must
    -- survive the migration for feature parity.
    description         TEXT,
    payment_method      TEXT,
    company             TEXT,
    car_no_plate        TEXT,
    paid_by             TEXT,

    -- --- user-owned --------------------------------------------------------
    category            TEXT,
    verified            BOOLEAN NOT NULL DEFAULT false,
    deleted_at          TIMESTAMPTZ,

    created_by          TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT transactions_confidence_range
        CHECK (confidence BETWEEN 0 AND 100),

    CONSTRAINT transactions_amount_positive
        CHECK (parsed_amount IS NULL OR parsed_amount > 0),

    -- A whatsapp-sourced row must point at the message it came from; manual and
    -- imported rows must not.
    CONSTRAINT transactions_source_linkage CHECK (
        (source = 'whatsapp' AND raw_message_id IS NOT NULL)
        OR (source <> 'whatsapp')
    ),

    CONSTRAINT transactions_import_linkage CHECK (
        (source = 'import') = (import_source_id IS NOT NULL)
    )
);

CREATE TRIGGER transactions_touch
    BEFORE UPDATE ON banksms.transactions
    FOR EACH ROW EXECUTE FUNCTION banksms.touch_updated_at();

-- One transaction per raw message. Without this, a re-parse or a replayed poll
-- batch could silently double-count money.
CREATE UNIQUE INDEX transactions_raw_message_id_key
    ON banksms.transactions (raw_message_id)
    WHERE raw_message_id IS NOT NULL;

-- Makes the fleet_expenses migration idempotent.
CREATE UNIQUE INDEX transactions_import_source_id_key
    ON banksms.transactions (import_source_id)
    WHERE import_source_id IS NOT NULL;

CREATE INDEX transactions_occurred_at_idx
    ON banksms.transactions (parsed_occurred_at DESC)
    WHERE deleted_at IS NULL;

CREATE INDEX transactions_account_occurred_at_idx
    ON banksms.transactions (parsed_account, parsed_occurred_at DESC)
    WHERE deleted_at IS NULL;

CREATE INDEX transactions_counterparty_trgm_idx
    ON banksms.transactions USING gin (parsed_counterparty gin_trgm_ops);

-- The costs view's filters.
CREATE INDEX transactions_category_idx
    ON banksms.transactions (category) WHERE deleted_at IS NULL;
CREATE INDEX transactions_company_idx
    ON banksms.transactions (company) WHERE deleted_at IS NULL;
CREATE INDEX transactions_car_no_plate_idx
    ON banksms.transactions (car_no_plate) WHERE deleted_at IS NULL;
CREATE INDEX transactions_source_idx
    ON banksms.transactions (source) WHERE deleted_at IS NULL;

-- ---------------------------------------------------------------------------
-- transaction_overrides — per-field user corrections, append-only in practice.
-- Gives GET /transactions/:id/history a real answer and lets the UI show
-- "the SMS said X, you changed it to Y".
-- ---------------------------------------------------------------------------

CREATE TABLE banksms.transaction_overrides (
    id              BIGSERIAL PRIMARY KEY,
    transaction_id  BIGINT NOT NULL REFERENCES banksms.transactions(id) ON DELETE CASCADE,

    -- Name of the overridden logical field, e.g. 'amount', 'counterparty'.
    -- Validated against an allow-list in Rust, not here, so adding a field is
    -- a code change rather than a migration.
    field           TEXT NOT NULL,

    -- Rendered value. NULL is a meaningful override (it blanks the field), so
    -- is_cleared distinguishes "set to null" from "no override".
    value           TEXT,
    is_cleared      BOOLEAN NOT NULL DEFAULT false,

    actor           TEXT,
    set_at          TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- Only the newest row per (transaction, field) is effective; older rows are
    -- the audit trail.
    superseded_at   TIMESTAMPTZ
);

-- Exactly one live override per field per transaction.
CREATE UNIQUE INDEX transaction_overrides_live_key
    ON banksms.transaction_overrides (transaction_id, field)
    WHERE superseded_at IS NULL;

CREATE INDEX transaction_overrides_history_idx
    ON banksms.transaction_overrides (transaction_id, set_at DESC);

-- ---------------------------------------------------------------------------
-- notes
-- ---------------------------------------------------------------------------

CREATE TABLE banksms.notes (
    id              BIGSERIAL PRIMARY KEY,
    transaction_id  BIGINT NOT NULL REFERENCES banksms.transactions(id) ON DELETE CASCADE,
    body            TEXT NOT NULL,
    author          TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at      TIMESTAMPTZ,

    CONSTRAINT notes_body_not_blank CHECK (length(btrim(body)) > 0)
);

CREATE TRIGGER notes_touch
    BEFORE UPDATE ON banksms.notes
    FOR EACH ROW EXECUTE FUNCTION banksms.touch_updated_at();

CREATE INDEX notes_transaction_id_idx
    ON banksms.notes (transaction_id) WHERE deleted_at IS NULL;

-- ---------------------------------------------------------------------------
-- tags / transaction_tags
-- ---------------------------------------------------------------------------

CREATE TABLE banksms.tags (
    id          BIGSERIAL PRIMARY KEY,
    name        TEXT NOT NULL,
    color       TEXT,
    created_by  TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at  TIMESTAMPTZ,

    CONSTRAINT tags_name_not_blank CHECK (length(btrim(name)) > 0)
);

CREATE TRIGGER tags_touch
    BEFORE UPDATE ON banksms.tags
    FOR EACH ROW EXECUTE FUNCTION banksms.touch_updated_at();

-- Case-insensitive uniqueness among live tags.
CREATE UNIQUE INDEX tags_name_key
    ON banksms.tags (lower(name)) WHERE deleted_at IS NULL;

CREATE TABLE banksms.transaction_tags (
    transaction_id  BIGINT NOT NULL REFERENCES banksms.transactions(id) ON DELETE CASCADE,
    tag_id          BIGINT NOT NULL REFERENCES banksms.tags(id) ON DELETE CASCADE,
    tagged_by       TEXT,
    tagged_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (transaction_id, tag_id)
);

CREATE INDEX transaction_tags_tag_id_idx
    ON banksms.transaction_tags (tag_id);

-- ---------------------------------------------------------------------------
-- ingest_cursor — exactly one row, ever.
--
-- The cursor is composite: timestamp alone is unsafe because messages share
-- seconds and ordering is not guaranteed stable. It is advanced in the SAME
-- transaction as the batch insert, so a crash mid-batch replays rather than
-- skips.
-- ---------------------------------------------------------------------------

CREATE TABLE banksms.ingest_cursor (
    id                  SMALLINT PRIMARY KEY DEFAULT 1,
    chat_jid            TEXT,
    last_wa_timestamp   TIMESTAMPTZ,
    last_wa_message_id  TEXT,
    last_poll_at        TIMESTAMPTZ,
    last_error          TEXT,
    last_error_at       TIMESTAMPTZ,
    consecutive_errors  INTEGER NOT NULL DEFAULT 0,
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT ingest_cursor_singleton CHECK (id = 1)
);

INSERT INTO banksms.ingest_cursor (id) VALUES (1);

-- ---------------------------------------------------------------------------
-- parse_templates — DB-backed pattern registry.
--
-- Patterns are stored in POST-NORMALIZATION form (see the normalization pre-pass
-- in the parser): Arabic alef forms unified to ا, ى -> ي, ة -> ه, digits folded
-- to ASCII. A pattern containing raw `إلى` can never match a normalized body.
--
-- Pattern compilation is validated in Rust before INSERT/UPDATE, NOT by a CHECK
-- constraint: Postgres POSIX regex does not support the `(?P<name>...)` named
-- capture groups these patterns depend on, so a DB-side test match would reject
-- valid patterns.
-- ---------------------------------------------------------------------------

CREATE TABLE banksms.parse_templates (
    id              BIGSERIAL PRIMARY KEY,
    name            TEXT NOT NULL,
    pattern         TEXT NOT NULL,

    -- Ordered list of chrono format strings to try, e.g. ["%m/%d/%y"].
    -- Per-template, never shared: a shared date parser silently produces wrong
    -- dates rather than erroring, because the same string is valid in several
    -- formats with different meanings.
    date_formats    JSONB NOT NULL DEFAULT '[]'::jsonb,

    -- Maps a captured action word to a direction, e.g. {"debited":"out"}.
    direction_map   JSONB NOT NULL DEFAULT '{}'::jsonb,

    -- Lower runs first; lets a specific pattern pre-empt a general one.
    priority        INTEGER NOT NULL DEFAULT 100,

    enabled         BOOLEAN NOT NULL DEFAULT true,
    version         INTEGER NOT NULL DEFAULT 1,
    notes           TEXT,
    created_by      TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT parse_templates_name_not_blank CHECK (length(btrim(name)) > 0),
    CONSTRAINT parse_templates_pattern_not_blank CHECK (length(btrim(pattern)) > 0),
    CONSTRAINT parse_templates_date_formats_is_array CHECK (jsonb_typeof(date_formats) = 'array'),
    CONSTRAINT parse_templates_direction_map_is_object CHECK (jsonb_typeof(direction_map) = 'object')
);

CREATE TRIGGER parse_templates_touch
    BEFORE UPDATE ON banksms.parse_templates
    FOR EACH ROW EXECUTE FUNCTION banksms.touch_updated_at();

CREATE UNIQUE INDEX parse_templates_name_key ON banksms.parse_templates (name);

CREATE INDEX parse_templates_enabled_idx
    ON banksms.parse_templates (priority, id) WHERE enabled;

-- ---------------------------------------------------------------------------
-- noise_skeletons — skeletons a human confirmed are not transactions.
-- Future messages matching one auto-`ignore`, which is how the feedback loop
-- stops the review queue refilling with the same chatter.
-- ---------------------------------------------------------------------------

CREATE TABLE banksms.noise_skeletons (
    skeleton_hash   TEXT PRIMARY KEY,
    example_body    TEXT,
    marked_by       TEXT,
    marked_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
