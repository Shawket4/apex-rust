INSERT INTO banksms.categories (key, label, label_ar, posting_kind, required_party, sort_order)
VALUES ('SalaryPortion', 'Part of salary', 'جزء من الراتب', 'salary', 'either', 12)
ON CONFLICT (key) DO NOTHING;
UPDATE banksms.categories SET required_party = 'either' WHERE key = 'Salary';
