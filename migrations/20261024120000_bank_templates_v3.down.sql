-- Removes the v3 templates. Transactions they created stay: they are real
-- money movements, and their raw messages keep the template name.
DELETE FROM banksms.parse_templates
WHERE name IN (
  'bm_transfer_out', 'bm_transfer_in', 'instant_credit', 'instant_reversal',
  'wallet_transfer_v2', 'ipn_merchant', 'nbe_card', 'card_merchant',
  'debit_card_en', 'credit_card_charge', 'credit_card_payment', 'abk_reversal',
  'ipn_received_en', 'ref_balance_v2', 'ipn_refund', 'internal_transfer_en'
);
