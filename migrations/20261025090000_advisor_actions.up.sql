-- The advisor writes to apex-rust's own data too (target 'apex'), and some
-- writes need more than method/path/body to apply safely: an If-Match version
-- for the bank-SMS ledger, or a check of maint-rust's per-operation results.
ALTER TABLE advisor.suggestions DROP CONSTRAINT suggestions_target_check;
ALTER TABLE advisor.suggestions
    ADD CONSTRAINT suggestions_target_check CHECK (target IN ('falcon', 'maint', 'apex'));
ALTER TABLE advisor.suggestions
    ADD COLUMN apply_meta JSONB NOT NULL DEFAULT '{}'::jsonb;

-- AI-made writes to apex-rust's own routes (X-Via-AI). The row is opened
-- before the handler runs and closed with the caller and status after, so an
-- AI write is never unrecorded. user_id/permission stay NULL when the request
-- failed authentication.
CREATE TABLE advisor.ai_audit_log (
    id             BIGSERIAL PRIMARY KEY,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at    TIMESTAMPTZ,
    user_id        BIGINT,
    permission     INT,
    via            TEXT NOT NULL,
    suggestion_id  UUID,
    method         TEXT NOT NULL,
    path           TEXT NOT NULL,
    status         INT
);
CREATE INDEX ai_audit_log_created_idx ON advisor.ai_audit_log (created_at DESC);
CREATE INDEX ai_audit_log_suggestion_idx ON advisor.ai_audit_log (suggestion_id);
