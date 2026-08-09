-- Remove the verified flag.
--
-- Verification was a human confirming that a parsed transaction matched its
-- bank message. A tier-1 template match already establishes that: the pattern
-- either matched the message exactly or it did not, and a person re-reading the
-- same SMS adds no information the parser did not already have.
--
-- It also gated posting to public.loans, which meant an advance sat inert until
-- someone clicked. That gate is gone: posting is now driven by the category and
-- the party, which are the things a human actually decides.

DROP INDEX IF EXISTS banksms.transactions_verified_idx;

ALTER TABLE banksms.transactions DROP COLUMN IF EXISTS verified;
