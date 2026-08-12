-- Make a human review decision terminal, and remove notes.
--
-- The bug: reclassifying a message as "it IS a transaction" only set it back to
-- `pending` for the parser to try again. The parser is deterministic, so it
-- reached the same conclusion and the message returned to the queue immediately
-- -- forever. A message that parses only partially therefore appeared in the
-- transactions list AND stayed in the review queue, and no amount of clicking
-- could resolve it.
--
-- The queue's exit condition was "the parser is now happy", which a human has no
-- way to bring about. It should be "a human has dealt with this".

ALTER TABLE banksms.raw_messages
    ADD COLUMN IF NOT EXISTS reviewed_at  TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS reviewed_by  TEXT;

-- The queue reads this, so it is worth an index even at small volumes.
CREATE INDEX IF NOT EXISTS raw_messages_unreviewed_idx
    ON banksms.raw_messages (parse_status)
    WHERE reviewed_at IS NULL;

-- Anything that already produced a transaction has effectively been accepted:
-- the money is in the ledger. Leaving these queued would ask the operator to
-- re-decide something the system already acted on.
UPDATE banksms.raw_messages r
SET reviewed_at = now(), reviewed_by = 'migration'
FROM banksms.transactions t
WHERE t.raw_message_id = r.id
  AND r.reviewed_at IS NULL
  AND r.parse_status IN ('partial', 'unmatched');

-- ---------------------------------------------------------------------------
-- Notes are removed.
--
-- `transactions.description` already holds free text about a transaction, and
-- two places to write the same thing means neither is trusted and reports have
-- to read both. The table is days old and holds nothing of record.
-- ---------------------------------------------------------------------------

DROP TABLE IF EXISTS banksms.notes;
