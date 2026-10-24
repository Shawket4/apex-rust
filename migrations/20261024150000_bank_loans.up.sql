-- ============================================================================
-- Bank loans: the installments a transfer can be checked against.
--
-- Some loans are paid through a person (a relative pays them at the branch),
-- so a transfer to that person may be an installment or may be something else
-- entirely. The loan sheet tells them apart: what fell due around the time of
-- the transfer, and for how much.
--
-- The rows come from the owner's loan sheet with
-- `apex-rust import-loans FILE.json` (the file replaces the schedule), never
-- from a migration: the figures are private.
-- ============================================================================

CREATE TABLE banksms.bank_loans (
  id           BIGSERIAL PRIMARY KEY,
  lender       TEXT NOT NULL,
  name         TEXT NOT NULL,
  installment  NUMERIC(14,2) NOT NULL CHECK (installment > 0),
  -- The day of the month it falls due; a shorter month takes its last day.
  due_day      SMALLINT NOT NULL CHECK (due_day BETWEEN 1 AND 31),
  -- The first and the last installment. No first: running since before the
  -- ledger begins.
  first_due    DATE,
  last_due     DATE NOT NULL,
  -- The one payee it is always paid through (a loan account the bank names,
  -- the relative who takes it to the branch). None: any payee who pays loans.
  payee_id     BIGINT REFERENCES banksms.payees(id) ON DELETE SET NULL,
  notes        TEXT,
  created_by   TEXT,
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  CHECK (first_due IS NULL OR first_due <= last_due)
);

-- Transfers to this payee are often loan installments paid on the company's
-- behalf: each is checked against the installments due at the time.
ALTER TABLE banksms.payees ADD COLUMN pays_loans BOOLEAN NOT NULL DEFAULT false;
