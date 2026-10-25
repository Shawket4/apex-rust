DROP TABLE IF EXISTS advisor.ai_audit_log;
ALTER TABLE advisor.suggestions DROP COLUMN IF EXISTS apply_meta;
DELETE FROM advisor.suggestions WHERE target = 'apex';
ALTER TABLE advisor.suggestions DROP CONSTRAINT suggestions_target_check;
ALTER TABLE advisor.suggestions
    ADD CONSTRAINT suggestions_target_check CHECK (target IN ('falcon', 'maint'));
