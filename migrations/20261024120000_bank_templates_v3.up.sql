-- ===========================================================================
-- Bank-SMS templates v3: the formats that arrived after the 2026-08-12
-- corpus was frozen — Banque Misr (19888), NBE incoming transfers, merchant
-- and card debits (NBE 19623, CIB 19666, ABK 19322, the 19555 bank), the
-- English debit-card and credit-card notices, and the refund/reversal
-- notices. Checked against every bank message received through 2026-10-24:
-- all 148 money messages that went to 'ignored' now match, and nothing else
-- does (declined cards, statements, installment reminders, OTPs and promos
-- stay ignored).
--
-- Every row sits after the v2 templates in priority, so a message an older
-- template already matches keeps its template and its transaction. Run
-- `apex-rust reparse` after deploying to parse the messages that were
-- waiting (it only touches messages without a transaction).
--
-- Patterns are POST-normalization (الي not إلى, ه not ة, ASCII digits).
-- Samples are invented messages in each bank's exact wording: this
-- repository is public, so no real names, accounts or amounts.
--
-- date_formats markers: "@received" — the message carries no date at all,
-- and the transaction is dated by the message's arrival (these are real-time
-- notifications); "@12h" — a 12-hour clock with no AM/PM, resolved by the
-- arrival time. A date with no time takes the arrival time when the message
-- arrived that same day, midnight otherwise.
--
-- ON CONFLICT DO NOTHING: a template someone already added by hand under the
-- same name is left alone.
-- ===========================================================================

INSERT INTO banksms.parse_templates (name, pattern, date_formats, direction_map, sample, priority, notes) VALUES

('bm_transfer_out',
 $rx$تم\s+(?P<action_ar>تحويل)\s+مبلغ\s+(?P<amount>[\d,]+(?:\.\d+)?)\s*(?P<currency>[A-Z]{3}|جم)\s+من\s+حساب\s+رقم\s+[xX*]*(?P<account>\d{3,6})\s+في\s+(?P<date>\d{1,2}-[A-Za-z]{3}-\d{4})\s+عن\s+طريق\s+التحويل\s+اللحظي$rx$,
 '["%d-%b-%Y"]', '{"تحويل":"out"}',
 'تم تحويل مبلغ 2500EGP     من حساب رقم xxx6666       فى 05-SEP-2026  عن طريق التحويل اللحظي',
 50, 'Banque Misr (19888). No payee, no reference, no time. Many of these were PetroApp top-ups before BM started naming the payee (see wallet_transfer_v2) — the text cannot tell.'),

('bm_transfer_in',
 $rx$تم\s+(?P<action_ar>اضافه)\s+مبلغ\s+(?P<amount>[\d,]+(?:\.\d+)?)\s*(?P<currency>[A-Z]{3}|جم)\s+الي\s+حساب\s+رقم\s+[xX*]*(?P<account>\d{3,6})\s+في\s+(?P<date>\d{1,2}-[A-Za-z]{3}-\d{4})\s+عن\s+طريق\s+التحويل\s+اللحظي$rx$,
 '["%d-%b-%Y"]', '{"اضافه":"in"}',
 'تم اضافة مبلغ 15000EGP     الى حساب رقم xxx6666      فى 05-SEP-2026  عن طريق التحويل اللحظي',
 51, 'Banque Misr (19888). Incoming, no sender, no time.'),

('instant_credit',
 $rx$تم\s+اضافه\s+تحويل\s+(?P<action_ar>لحظي)\s+لحسابكم\s+رقم\s+[xX*]*(?P<account>\d{3,6})\s+بمبلغ\s+(?P<amount>[\d,]+(?:\.\d+)?)\s+(?P<currency>جم|ج\.م|[A-Z]{3})\s+من\s+(?P<counterparty>.+?)\s+رقم\s+مرجعي\s+(?P<reference>[A-Za-z0-9]+)\s+يوم\s+(?P<date>(?:\d{4}-)?\d{2}-\d{2})\s+الساعه\s*(?P<time>\d{1,2}:\d{2})$rx$,
 '["%Y-%m-%d","%m-%d"]', '{"لحظي":"in"}',
 'تم إضافة تحويل لحظي لحسابكم رقم 5555 بمبلغ 3500.00 جم من KARIM NABIL FAHMY رقم مرجعي 418273645091 يوم 09-14 الساعة 11:20 للمزيد اتصل بـ 19623',
 52, 'Incoming instant transfer. NBE (19623) writes a year-less MONTH-FIRST date like wallet_transfer; Banque Misr (19888) writes the year (2026-10-02).'),

('instant_reversal',
 $rx$تم\s+(?P<action_ar>رد)\s+تحويل\s+لحظي\s+لحسابكم\s+رقم\s+[xX*]*(?P<account>\d{3,6})\s+بمبلغ\s+(?P<amount>[\d,]+(?:\.\d+)?)\s+(?P<currency>جم|ج\.م|[A-Z]{3})\s+رقم\s+مرجعي\s+(?P<reference>[A-Za-z0-9]+)\s+يوم\s+(?P<date>(?:\d{4}-)?\d{2}-\d{2})\s+الساعه\s*(?P<time>\d{1,2}:\d{2})$rx$,
 '["%Y-%m-%d","%m-%d"]', '{"رد":"in"}',
 'تم رد تحويل لحظي لحسابكم رقم 5555 بمبلغ 5.00 جم رقم مرجعي 902817364512 يوم 09-14 الساعة 12:05 للمزيد اتصل بـ 19623',
 53, 'NBE (19623) returned transfer. The reference is the original transfer''s, so the pair can be matched.'),

('wallet_transfer_v2',
 $rx$تم\s+تنفيذ\s+تحويل\s+(?P<action_ar>لحظي)\s+من\s+حسابكم\s+رقم\s+[xX*]*(?P<account>\d{3,6})\s+بمبلغ\s+(?P<amount>[\d,]+(?:\.\d+)?)\s+(?P<currency>جم|ج\.م|[A-Z]{3})\s+الي\s+(?P<counterparty>.+?)\s+رقم\s+مرجعي\s+(?P<reference>[A-Za-z0-9]+)\s+يوم\s+(?P<date>(?:\d{4}-)?\d{2}-\d{2})\s+الساعه\s*(?P<time>\d{1,2}:\d{2})$rx$,
 '["%Y-%m-%d","%m-%d"]', '{"لحظي":"out"}',
 'تم تنفيذ تحويل لحظي من حسابكم رقم xxx6666 بمبلغ 2500 جم إلى Hany Fawzy رقم مرجعي 551203948776 يوم 2026-10-02 الساعه 10:15 للمزيد اتصل ب 19888 ',
 54, 'wallet_transfer as Banque Misr (19888) writes it: xxx account, whole amounts, dated with the year (2026-10-02). PetroApp payees are suppressed like the original.'),

('ipn_merchant',
 $rx$تم\s+(?P<action_ar>خصم)\s+مبلغ\s+(?P<amount>[\d,]+(?:\.\d+)?)\s+(?P<currency>جم|ج\.م|[A-Z]{3})\s+لحظيا\s+باستخدام\s+\(?شبكه\s+المدفوعات\s+اللحظيه\)?\s+من\s+حسابكم\s+رقم\s+[xX*]*(?P<account>\d{3,6})\s+عند\s+(?P<counterparty>.+?)\s+يوم\s+(?P<date>(?:\d{4}-)?\d{2}-\d{2})\s+الساعه\s*(?P<time>\d{1,2}:\d{2})(?:\s+مرجع\s+التاجر\s+(?P<reference>[A-Za-z0-9]+))?$rx$,
 '["%Y-%m-%d","%m-%d"]', '{"خصم":"out"}',
 'تم خصم مبلغ 250.75 جم لحظيا باستخدام (شبكة المدفوعات اللحظية) من حسابكم رقم 5555 عند Corner Market يوم 09-14 الساعة16:40 مرجع التاجر 1600000001 للمزيد اتصل بـ 19623.',
 55, 'NBE (19623) IPN payment to a merchant (installments, mobile recharge). Year-less month-first date.'),

('nbe_card',
 $rx$تم\s+(?P<action_ar>خصم|سحب)\s+(?P<amount>[\d,]+(?:\.\d+)?)\s*(?P<currency>[A-Z]{3}|جم)\s+من\s+بطاقه\s+الخصم\s+المباشر\s+رقم\s*\**(?P<account>\d{4})\s+عند\s*(?P<counterparty>.+?)\s+يوم\s*(?P<date>\d{1,2}/\d{1,2}/\d{2})\s+الساعه\s*(?P<time>\d{1,2}:\d{2})$rx$,
 '["%d/%m/%y"]', '{"خصم":"out","سحب":"out"}',
 'تم خصم 450 EGP  من بطاقة الخصم المباشر رقم7777  عندATM 0042 MAIN ST     يوم14/09/26  الساعه09:05  المتاح1520.40EGP  للمزيد إتصل ب ١٩٦٢٣',
 56, 'NBE (19623) debit card, POS and ATM. Spaces after رقم/عند/يوم/الساعه are optional (ATM messages drop them). Card number is the account.'),

('card_merchant',
 $rx$تم\s+(?P<action_ar>خصم|سحب)\s+(?:مبلغ\s+)?(?P<currency>[A-Z]{3})\s*(?P<amount>[\d,]+(?:\.\d+)?)\s+من\s+بطاقه\s+الخصم\s+المباشر\s+(?:المنتهيه\s+ب|#)\s*\**(?P<account>\d{4})\s+(?:باستخدام\s+.+?\s+)?عند\s+(?P<counterparty>.+?)\s+في\s+(?P<date>\d{1,2}/\d{1,2}/\d{2})\s+(?P<time>\d{1,2}:\d{2})$rx$,
 '["%d/%m/%y"]', '{"خصم":"out","سحب":"out"}',
 'تم خصم مبلغ  EGP 320.00 من بطاقة الخصم المباشر المنتهية بـ **4444 عند NILE FUEL STATION في 14/09/26 13:24 ، الرصيد المتاح 1834.10 EGP',
 57, 'CIB (19666) debit-card purchase — "عند <merchant>" where cib_card has "من <branch>". Also the Apple Pay variant ("# **4444 باستخدام Apple Pay عند ...").'),

('debit_card_en',
 $rx$Your\s+debit\s+card\s+\**(?P<account>\d{4})\s+was\s+(?P<action>debited|credited)\s+by\s+(?P<currency>[A-Z]{3})\s*(?P<amount>[\d,]+(?:\.\d+)?)\s+at\s+(?P<counterparty>.+?)(?:\s+EG)?\.\s+Available\s+balance$rx$,
 '["@received"]', '{"debited":"out","credited":"in"}',
 'Your debit card **8888 was debited by EGP 1,250.00 at DELTA PHARMACY 1001      Giza EG. Available balance is EGP 4,310.25.',
 58, 'English debit-card notice (AAIB ATMs appear as "AAIB NULL"). No date in the text: dated by arrival. The trailing country code is dropped from the merchant.'),

('credit_card_charge',
 $rx$Your\s+Credit\s+Card\s*#?\s*\**(?P<account>\d{4})\s+was\s+(?P<action>charged)\s+(?P<currency>[A-Z]{3})\s*(?P<amount>[\d,]+(?:\.\d+)?)\s+at\s+(?P<counterparty>.+?)\s+on\s+(?P<date>\d{1,2}/\d{1,2}/\d{2})\s+at\s+(?P<time>\d{1,2}:\d{2})$rx$,
 '["%d/%m/%y","@12h"]', '{"charged":"out"}',
 'Your Credit Card#**3434 was charged EGP 49.99 at STREAMING SVC on 14/09/26 at 09:10. Your available limit is EGP 20000. For more info, call 19322.',
 59, 'ABK (19322) credit-card charge. DAY-first date here, unlike ABK account messages. 12-hour clock without AM/PM ("at 08:15" arriving 20:17): the arrival picks the reading.'),

('credit_card_payment',
 $rx$Your\s+[\w\-]+\s+Credit\s+Card\s+\**(?P<account>\d{4})\s+was\s+(?P<action>credited|debited)\s+with\s+(?P<currency>[A-Z]{3})\s*(?P<amount>[\d,]+(?:\.\d+)?)\s+on\s+(?P<date>\d{1,2}/\d{1,2}/\d{2,4})\s+(?P<time>\d{1,2}:\d{2})\s+(?:with|for)\s+IPN\s+transfer\s+(?:from|to)\s+(?P<counterparty>.+?)\s*\(ID\s+(?P<reference>\w+)\)$rx$,
 '["%m/%d/%y","%m/%d/%Y"]', '{"credited":"in","debited":"out"}',
 'Your ABK-Egypt Credit Card *3434 was credited with EGP 2500.00 on 9/14/26 10:00 with IPN transfer from OMAR TAREK SALEM (ID 7a7b7c7d).For queries call 19322',
 60, 'ABK (19322) credit-card repayment by IPN. MONTH-first, like the abk template.'),

('abk_reversal',
 $rx$(?P<action>Reversal)\s+of\s+debit\s+of\s+(?P<amount>[\d,]+(?:\.\d+)?)\s*(?P<currency>[A-Z]{3})\s+in\s+your\s+[\w\-]+\s+account\s+\*(?P<account>\d{3,6})\s+on\s+(?P<date>\d{1,2}/\d{1,2}/\d{2,4})\s+(?P<time>\d{1,2}:\d{2})\s+for\s+(?:InstaPay|IPN)\s+transfer\s+to\s+(?P<counterparty>.+?)\s*\(ID\s+(?P<reference>\w+)\)$rx$,
 '["%m/%d/%y","%m/%d/%Y"]', '{"Reversal":"in"}',
 'Reversal of debit of 64.0 EGP in your ABK-Egypt account *3333 on 9/14/26 18:40 for InstaPay transfer to وليد حازم عزت (ID 1a2b3c4d).For queries call 19322',
 61, 'ABK (19322) reversed transfer. Same (ID ...) as the original debit; the counterparty is the original payee.'),

('ipn_received_en',
 $rx$You\s+have\s+(?P<action>received)\s+a\s+transfer\s+reference\s+#(?P<reference>\w*)\s+with\s+(?P<currency>[A-Z]{3})\s*(?P<amount>[\d,]+(?:\.\d+)?)\s+from\s+(?P<counterparty>.+?)\s+through\s+IPN\s+on\s+(?P<date>\d{2}/\d{2}/\d{4})\s+at\s+(?P<time>\d{1,2}:\d{2})$rx$,
 '["%d/%m/%Y"]', '{"received":"in"}',
 'You have received a transfer reference #9c8b7a65 with EGP 1500 from TAREK ADEL MANSOUR through IPN on 14/09/2026 at 15:30. For inquiries, call 19555.',
 62, 'The 19555 bank (ref_balance), incoming. The text names no account.'),

('ref_balance_v2',
 $rx$(?:Transfer|Purchase)\s+reference\s+#(?P<reference>\w*)\s+of\s+(?P<currency>[A-Z]{3})\s*(?P<amount>[\d,]+(?:\.\d+)?)\s+has\s+been\s+(?P<action>debited|credited)\s+(?:from|to)\s+your\s+account\s+(?P<account>[\d\-]+)\s+through\s+IPN\s+on\s+(?P<date>\d{2}/\d{2}/\d{4})\s+at\s+(?P<time>\d{1,2}:\d{2})$rx$,
 '["%d/%m/%Y"]', '{"debited":"out","credited":"in"}',
 'Purchase reference #a1b2c3d4 of EGP 85.5 has been debited from your account 9001-01 through IPN on 14/09/2026 at 17:38, your available balance is 912.40. For inquiries, call 19555.',
 63, 'ref_balance plus what it misses: "Purchase reference", one-decimal amounts, and the empty reference on the separately charged 0.5 IPN fee.'),

('ipn_refund',
 $rx$عمليه\s+التحويل\s+التي\s+تمت\s+عبر\s+خدمات\s+الانترنت\s+البنكيه\s+من\s+حساب\s+\**(?P<account>\d{3,6})\s+ب\s*(?P<currency>[A-Z]{3})\s*(?P<amount>[\d,]+(?:\.\d+)?)\s+في\s+(?P<date>\d{2}-\d{2}-\d{4})\s+(?P<time>\d{1,2}:\d{2})\s+الي\s+(?P<counterparty>.+?)\s+برقم\s+مرجعي\s+(?P<reference>[A-Za-z0-9]+)\s+لم\s+يتم\s+تنفيذها.*?تم\s+(?P<action_ar>رد)\s+المبلغ$rx$,
 '["%d-%m-%Y"]', '{"رد":"in"}',
 'عميلنا العزيز، يرجى العلم أن عملية التحويل التي تمت عبر خدمات الإنترنت البنكية من حساب ********2222 بـ EGP1500.00 فى 14-09-2026 18:34 إلى Ayman K........ T... برقم مرجعي FT26257ABCDE لم يتم تنفيذها لحظيًا من خلال شبكة المدفوعات اللحظية (IPN)  وقد تم رد المبلغ إلى حسابكم.',
 64, 'CIB (19666) failed IPN transfer, money returned. Same FT reference as the arabic_ipn debit it undoes; the counterparty is the original payee.'),

('internal_transfer_en',
 $rx$You\s+have\s+(?P<action>initiated)\s+an\s+Internal\s+Account\s+Transfer\s+transaction\s+from\s+account\s+[xX*]*\d*?(?P<account>\d{4})\s+to\s+payee\s+(?P<counterparty>.+?)\s+with\s+the\s+amount\s+of\s+(?P<currency>[A-Z]{3})\s*(?P<amount>[\d,]+(?:\.\d+)?)$rx$,
 '["@received"]', '{"initiated":"out"}',
 'Dear Customer, You have initiated an Internal Account Transfer transaction from account XXXXXXXXXXXX1234567 to payee Hany Fawzy with the amount of EGP12,500',
 65, 'Same-bank transfer to a saved payee. No date: dated by arrival. The account is the last four digits.')

ON CONFLICT (name) DO NOTHING;
