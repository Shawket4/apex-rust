-- Reverses 20260808120100_seed_parse_templates.up.sql.
-- Scoped by name so a hand-added template is never collateral damage.

DELETE FROM banksms.parse_templates
WHERE name IN ('arabic_ipn', 'cib_card', 'abk', 'ref_balance')
  AND created_by = 'migration';
