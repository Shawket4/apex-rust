-- Splitting: one bank SMS often pays for several things (15,000 advance to a
-- driver + 5,000 parts in one transfer). A split creates ordinary child rows
-- that carry the categories/people, while the parent keeps the verbatim
-- message link and the full amount and leaves the ledger. Children must sum
-- EXACTLY to the parent — enforced by the API inside one transaction, since a
-- CHECK cannot see across rows.

ALTER TABLE banksms.transactions
    ADD COLUMN parent_id BIGINT REFERENCES banksms.transactions(id),
    ADD COLUMN split_at  TIMESTAMPTZ;

-- Children are their own source kind: 'manual' would lie about provenance
-- (the money came from the bank) and 'whatsapp' would demand a raw link the
-- parent already owns.
ALTER TABLE banksms.transactions DROP CONSTRAINT transactions_source_check;
ALTER TABLE banksms.transactions
    ADD CONSTRAINT transactions_source_check
        CHECK (source IN ('whatsapp','import','manual','split')),
    ADD CONSTRAINT split_has_parent CHECK ((source = 'split') = (parent_id IS NOT NULL)),
    ADD CONSTRAINT child_has_no_raw CHECK (parent_id IS NULL OR raw_message_id IS NULL);

CREATE INDEX transactions_parent_idx ON banksms.transactions (parent_id)
    WHERE parent_id IS NOT NULL;
