-- Sixth template: wallet/instant transfer with a year-less date. Hotline 19623.
--
-- Distinct from instant_transfer despite the shared opening words:
--   * the account comes BEFORE the amount ("من حسابكم رقم 0180 بمبلغ ...")
--   * it carries a counterparty, introduced by إلى
--   * the reference is numeric and introduced by "رقم مرجعي" without the ب
--   * THE DATE HAS NO YEAR: "يوم 08-09 الساعة 14:15"
--
-- %m-%d is resolved against the message's own arrival timestamp (see
-- parse_yearless in parser/templates.rs). Guessing "this year" would be right
-- until an old message was reparsed, and a wrong year on a transfer is worse
-- than no date at all.
--
-- Messages matching this template are usually PetroApp top-ups, which the
-- parser suppresses so fuel is not counted twice -- once here and once as a
-- fuel_event from the PetroApp sync. The template still exists so the format is
-- RECOGNISED rather than unknown: an unrecognised message is a gap to chase, a
-- suppressed one is a decision.

INSERT INTO banksms.parse_templates
    (name, pattern, date_formats, direction_map, priority, created_by, notes)
VALUES (
    'wallet_transfer',
    $re$تم\s+تنفيذ\s+تحويل\s+(?P<action_ar>لحظي)\s+من\s+حسابكم\s+رقم\s+\**(?P<account>\d{3,6})\s+بمبلغ\s+(?P<amount>[\d,]+\.\d{2})\s+(?P<currency>جم|ج\.م|[A-Z]{3})\s+الي\s+(?P<counterparty>.+?)\s+رقم\s+مرجعي\s+(?P<reference>[A-Za-z0-9]+)\s+يوم\s+(?P<date>\d{2}-\d{2})\s+الساعه\s+(?P<time>\d{1,2}:\d{2})$re$,
    '["%m-%d"]'::jsonb,
    '{"لحظي":"out"}'::jsonb,
    16,
    'migration',
    'Hotline 19623. Year-less date resolved from the message timestamp. Usually PetroApp, which is suppressed.'
);
