-- Seed the parse_templates registry.
--
-- These four patterns were validated against the live chat history (479 text
-- messages from the target chat): 34 tier-1 matches, zero messages matched by
-- more than one template, zero direction words missing from a direction_map, and
-- every extracted date agreed with the WhatsApp envelope timestamp after
-- Africa/Cairo conversion.
--
-- IMPORTANT: patterns are in POST-NORMALIZATION form. The parser normalizes a
-- copy of the body before matching (Arabic-Indic digits -> ASCII, alef forms
-- unified to ا, ى -> ي, ة -> ه, tatweel and diacritics stripped, bidi and
-- zero-width chars removed, whitespace collapsed, NFKC last). So the Arabic here
-- reads `الي` not `إلى`, `اضافه` not `إضافة`, `بطاقه` not `بطاقة`. Writing a
-- pattern in raw form is the single easiest way to make it silently never match.
--
-- `date_formats` is per-template and deliberately NOT shared. `8/6/26` is a
-- valid date under both %m/%d/%y and %d/%m/%y with different meanings, so a
-- shared date parser produces wrong dates rather than errors.
--
-- `priority` orders matching: lower runs first.

INSERT INTO banksms.parse_templates
    (name, pattern, date_formats, direction_map, priority, created_by, notes)
VALUES

-- Arabic IPN transfer notification. Hotline 19666. 10 matches.
-- Accounts appear as ********9276 (8 asterisks + 4 digits).
(
    'arabic_ipn',
    $re$تم\s+(?P<action_ar>خصم|اضافه|ايداع)\s+مبلغ\s+(?P<currency>[A-Z]{3})\s+(?P<amount>[\d,]+\.\d{2})\s+من\s+حساب\s+\**(?P<account>\d{3,6})\b.*?بتاريخ\s+(?P<date>\d{2}-\d{2}-\d{4})\s+(?P<time>\d{2}:\d{2})\s+الي\s+(?P<counterparty>.+?)\s+برقم\s+مرجعي\s+(?P<reference>[A-Z0-9]+)$re$,
    '["%d-%m-%Y"]'::jsonb,
    '{"خصم":"out","اضافه":"in","ايداع":"in"}'::jsonb,
    10,
    'migration',
    'Hotline 19666. Reference format FT + alphanumeric. Counterparty is partially masked.'
),

-- CIB direct-debit card withdrawal. NOT in the original spec; found during recon.
-- Structurally distinct: a CARD mask (**2234) rather than an account mask, a
-- merchant/branch string instead of a counterparty name, and NO reference number
-- at all -- so parsed_reference is legitimately NULL on a confidence-100 tier-1
-- match. Fourth distinct date format. 1 match.
(
    'cib_card',
    $re$تم\s+(?P<action_ar>سحب|خصم)\s+مبلغ\s+(?P<currency>[A-Z]{3})\s+(?P<amount>[\d,]+\.\d{2})\s+من\s+بطاقه\s+الخصم\s+المباشر\s+المنتهيه\s+ب\s+\**(?P<account>\d{4})\s+من\s+(?P<counterparty>.+?)\s+في\s+(?P<date>\d{1,2}/\d{1,2}/\d{2})\s+(?P<time>\d{1,2}:\d{2})\s*،\s*الرصيد\s+المتاح\s+(?:[A-Z]{3}\s+)?(?P<balance>[\d,]+\.\d{2})$re$,
    '["%d/%m/%y"]'::jsonb,
    '{"سحب":"out","خصم":"out"}'::jsonb,
    20,
    'migration',
    'CIB debit-card withdrawal. No reference number exists in this format. Counterparty is a branch/merchant.'
),

-- ABK-Egypt IPN transfer. Hotline 19322. 11 matches, including the only
-- `credited` (inbound) message in the corpus -- keep both directions working.
-- Amounts here carry 1 or 2 decimal places (500.5, 1001.0), unlike the others.
(
    'abk',
    $re$Your\s+(?P<bank>[\w\-]+)\s+account\s+\*(?P<account>\d{3,6})\s+was\s+(?P<action>credited|debited)\s+with\s+(?P<currency>[A-Z]{3})\s+(?P<amount>[\d,]+(?:\.\d+)?)\s+on\s+(?P<date>\d{1,2}/\d{1,2}/\d{2,4})\s+(?P<time>\d{1,2}:\d{2})\s+(?:with|for)\s+IPN\s+transfer\s+(?P<preposition>from|to)\s+(?P<counterparty>.+?)\s*\(ID\s+(?P<reference>\w+)\)$re$,
    '["%m/%d/%y","%m/%d/%Y"]'::jsonb,
    '{"debited":"out","credited":"in"}'::jsonb,
    30,
    'migration',
    'Hotline 19322. MONTH-FIRST dates (%m/%d/%y) -- the only template that is. Reference is the (ID ...) value.'
),

-- Transfer-reference + running balance. Hotline 19555. 12 matches.
-- Account is dashed (6001-01), not masked. Reports available balance.
(
    'ref_balance',
    $re$Transfer\s+reference\s+#(?P<reference>\w+)\s+of\s+(?P<currency>[A-Z]{3})\s+(?P<amount>[\d,]+\.\d{2})\s+has\s+been\s+(?P<action>debited|credited)\s+from\s+your\s+account\s+(?P<account>[\d\-]+)\s+through\s+IPN\s+on\s+(?P<date>\d{2}/\d{2}/\d{4})\s+at\s+(?P<time>\d{2}:\d{2}),\s*your\s+available\s+balance\s+is\s+(?P<balance>[\d,]+\.\d{2})$re$,
    '["%d/%m/%Y"]'::jsonb,
    '{"debited":"out","credited":"in"}'::jsonb,
    40,
    'migration',
    'Hotline 19555. Reference is lowercase hex. Balance drift here is how fee-exclusive amounts are detectable.'
);
