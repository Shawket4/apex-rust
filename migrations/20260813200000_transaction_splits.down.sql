DELETE FROM banksms.transactions WHERE source = 'split';
ALTER TABLE banksms.transactions
    DROP CONSTRAINT split_has_parent,
    DROP CONSTRAINT child_has_no_raw,
    DROP CONSTRAINT transactions_source_check;
ALTER TABLE banksms.transactions
    ADD CONSTRAINT transactions_source_check
        CHECK (source IN ('whatsapp','import','manual'));
ALTER TABLE banksms.transactions DROP COLUMN parent_id, DROP COLUMN split_at;
