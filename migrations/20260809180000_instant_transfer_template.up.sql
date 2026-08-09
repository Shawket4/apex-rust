-- ---------------------------------------------------------------------------
-- Fifth template: instant transfer (تحويل لحظي), hotline 19666.
--
-- Structurally distinct from arabic_ipn despite sharing a bank:
--   * the amount is introduced by "بمبلغ" after "تم تنفيذ تحويل لحظي",
--     not by "تم خصم مبلغ"
--   * the currency is the Arabic abbreviation جم, not the ISO code
--   * the account reads "من حسابك المنتهي ب", not "من حساب"
--   * the reference comes BEFORE the date, and is lowercase hex rather than
--     the FT-prefixed uppercase form
--   * there is no counterparty at all
--
-- Verified against the real message: matches it, and neither collides with the
-- four existing templates nor matches any of their samples.
-- ---------------------------------------------------------------------------

INSERT INTO banksms.parse_templates
    (name, pattern, date_formats, direction_map, priority, created_by, notes)
VALUES (
    'instant_transfer',
    $re$تم\s+تنفيذ\s+تحويل\s+(?P<action_ar>لحظي)\s+بمبلغ\s+(?P<amount>[\d,]+\.\d{2})\s+(?P<currency>جم|ج\.م|[A-Z]{3})\s+من\s+حسابك\s+المنتهي\s+ب\s*\**(?P<account>\d{3,6})\b.*?برقم\s+مرجعي\s+(?P<reference>[A-Za-z0-9]+)\s+بتاريخ\s+(?P<date>\d{2}-\d{2}-\d{4})\s+(?P<time>\d{2}:\d{2})$re$,
    '["%d-%m-%Y"]'::jsonb,
    '{"لحظي":"out"}'::jsonb,
    15,
    'migration',
    'Hotline 19666. Currency written as جم. No counterparty. Reference precedes the date.'
)
ON CONFLICT (name) DO NOTHING;
