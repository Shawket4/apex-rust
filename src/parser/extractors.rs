//! Tier 2: independent field-level extractors.
//!
//! Run only when no template matched but triage said this looks like a bank SMS.
//! Each extractor is independent, so a message in an unknown format still yields
//! whatever can be recovered rather than nothing.
//!
//! Confidence is never 100 here -- that value is reserved for a deterministic
//! tier-1 template match.

use chrono::{DateTime, Utc};
use regex::Regex;
use rust_decimal::Decimal;
use std::sync::OnceLock;

use crate::parser::templates::{parse_datetime, parse_money};
use crate::parser::Direction;

#[derive(Debug, Clone, Default)]
pub struct Extracted {
    pub direction: Option<Direction>,
    pub amount: Option<Decimal>,
    pub currency: Option<String>,
    pub account: Option<String>,
    pub counterparty: Option<String>,
    pub reference: Option<String>,
    pub balance_after: Option<Decimal>,
    pub occurred_at: Option<DateTime<Utc>>,
}

impl Extracted {
    /// Amount AND date are the minimum viable row. Without both, there is nothing
    /// worth storing as a transaction.
    pub fn is_viable(&self) -> bool {
        self.amount.is_some() && self.occurred_at.is_some()
    }

    /// 0-100, weighted by which fields were recovered. Capped at 95 so an
    /// extractor result can never be mistaken for a template match.
    pub fn confidence(&self) -> i16 {
        let mut score = 0;
        if self.amount.is_some() {
            score += 30;
        }
        if self.occurred_at.is_some() {
            score += 25;
        }
        if self.currency.is_some() {
            score += 10;
        }
        if self.direction.is_some() {
            score += 15;
        }
        if self.account.is_some() {
            score += 10;
        }
        if self.reference.is_some() {
            score += 5;
        }
        if self.counterparty.is_some() {
            score += 5;
        }
        score.min(95)
    }
}

struct Pats {
    amount_with_currency: Regex,
    bare_amount: Regex,
    account: Regex,
    reference: Regex,
    date: Regex,
    time: Regex,
    balance: Regex,
    debit_word: Regex,
    credit_word: Regex,
}

fn pats() -> &'static Pats {
    static P: OnceLock<Pats> = OnceLock::new();
    P.get_or_init(|| Pats {
        amount_with_currency: Regex::new(r"\b(?P<cur>[A-Z]{3})\s*(?P<amt>[\d,]+(?:\.\d{1,2})?)\b")
            .unwrap(),
        bare_amount: Regex::new(r"\b(?P<amt>[\d,]{2,}(?:\.\d{2}))\b").unwrap(),
        account: Regex::new(r"\*{1,10}(?P<acct>\d{3,6})\b|\b(?P<dashed>\d{4}-\d{2})\b").unwrap(),
        reference: Regex::new(r"\b(?P<ref>FT[A-Z0-9]{6,})\b|#(?P<hex>[0-9a-fA-F]{6,})\b|\(ID\s+(?P<id>\w+)\)")
            .unwrap(),
        date: Regex::new(r"\b(?P<d>\d{1,2}[/-]\d{1,2}[/-]\d{2,4})\b").unwrap(),
        time: Regex::new(r"\b(?P<t>\d{1,2}:\d{2})\b").unwrap(),
        // "balance is 11720.54" / "الرصيد المتاح EGP 8579.76"
        balance: Regex::new(r"(?i)balance\D{0,20}?(?P<bal>[\d,]+\.\d{2})|الرصيد\s+المتاح\s*(?:[A-Z]{3}\s*)?(?P<bal_ar>[\d,]+\.\d{2})")
            .unwrap(),
        debit_word: Regex::new(r"(?i)\bdebited|withdrawn|purchase\b|خصم|سحب").unwrap(),
        credit_word: Regex::new(r"(?i)\bcredited|deposited|received\b|اضافه|ايداع").unwrap(),
    })
}

/// Run every extractor over a NORMALIZED body.
///
/// Date formats are ambiguous at this tier -- there is no template to say which
/// convention this bank uses -- so day-first is tried before month-first. That is
/// the more common convention in the observed corpus (3 of 4 templates), and the
/// resulting row is queued for human review anyway.
pub fn extract(normalized: &str) -> Extracted {
    let p = pats();
    let mut out = Extracted::default();

    if let Some(c) = p.amount_with_currency.captures(normalized) {
        out.currency = c.name("cur").map(|m| m.as_str().to_string());
        out.amount = c.name("amt").and_then(|m| parse_money(m.as_str()));
    } else if let Some(c) = p.bare_amount.captures(normalized) {
        out.amount = c.name("amt").and_then(|m| parse_money(m.as_str()));
    }

    if let Some(c) = p.account.captures(normalized) {
        out.account = c
            .name("acct")
            .or_else(|| c.name("dashed"))
            .map(|m| m.as_str().to_string());
    }

    if let Some(c) = p.reference.captures(normalized) {
        out.reference = c
            .name("ref")
            .or_else(|| c.name("hex"))
            .or_else(|| c.name("id"))
            .map(|m| m.as_str().to_string());
    }

    if let Some(c) = p.balance.captures(normalized) {
        out.balance_after = c
            .name("bal")
            .or_else(|| c.name("bal_ar"))
            .and_then(|m| parse_money(m.as_str()));
    }

    let date = p.date.captures(normalized).and_then(|c| c.name("d").map(|m| m.as_str().to_string()));
    let time = p.time.captures(normalized).and_then(|c| c.name("t").map(|m| m.as_str().to_string()));
    if let Some(d) = date {
        let formats = [
            "%d/%m/%Y".to_string(),
            "%d-%m-%Y".to_string(),
            "%d/%m/%y".to_string(),
            "%d-%m-%y".to_string(),
            "%m/%d/%Y".to_string(),
            "%m/%d/%y".to_string(),
        ];
        out.occurred_at = parse_datetime(&d, time.as_deref(), &formats);
    }

    // Debit wins ties: an outgoing transfer misfiled as incoming inverts the
    // sign of the money, and debits are ~97% of observed traffic.
    out.direction = if p.debit_word.is_match(normalized) {
        Some(Direction::Out)
    } else if p.credit_word.is_match(normalized) {
        Some(Direction::In)
    } else {
        None
    };

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::normalize::normalize;
    use rust_decimal_macros::dec;

    #[test]
    fn extracts_from_an_unknown_english_format() {
        // Deliberately not one of the seed templates.
        let s = normalize("ALERT: EGP 1250.75 was debited from card **9911 on 03/02/2026 at 09:15, ref FT26034ABCDE");
        let e = extract(&s);
        assert_eq!(e.amount, Some(dec!(1250.75)));
        assert_eq!(e.currency.as_deref(), Some("EGP"));
        assert_eq!(e.account.as_deref(), Some("9911"));
        assert_eq!(e.reference.as_deref(), Some("FT26034ABCDE"));
        assert_eq!(e.direction, Some(Direction::Out));
        assert!(e.is_viable());
        assert!(e.confidence() < 100, "tier 2 must never reach 100");
    }

    #[test]
    fn extracts_from_an_unknown_arabic_format() {
        let s = normalize("إشعار: تم إضافة مبلغ EGP 5000.00 إلى حسابك ********1234 بتاريخ 15/03/2026 11:20");
        let e = extract(&s);
        assert_eq!(e.amount, Some(dec!(5000.00)));
        assert_eq!(e.direction, Some(Direction::In));
        assert_eq!(e.account.as_deref(), Some("1234"));
        assert!(e.is_viable());
    }

    #[test]
    fn missing_amount_or_date_is_not_viable() {
        let e = extract(&normalize("your account was debited today"));
        assert!(!e.is_viable());
    }

    #[test]
    fn confidence_rises_with_recovered_fields() {
        let sparse = extract(&normalize("EGP 100.00 on 01/02/2026"));
        let rich = extract(&normalize(
            "EGP 100.00 debited from **1234 on 01/02/2026 at 10:00 ref FT26032ABCDE balance is 500.00",
        ));
        assert!(rich.confidence() > sparse.confidence());
        assert!(rich.confidence() <= 95);
    }

    #[test]
    fn extracts_balance_in_both_languages() {
        let en = extract(&normalize("debited EGP 85.00 on 01/02/2026 10:00, your available balance is 11720.54"));
        assert_eq!(en.balance_after, Some(dec!(11720.54)));

        let ar = extract(&normalize("تم سحب مبلغ EGP 8000.00 في 08/08/26 10:32 ، الرصيد المتاح EGP 8579.76"));
        assert_eq!(ar.balance_after, Some(dec!(8579.76)));
    }

    #[test]
    fn debit_wins_when_both_words_appear() {
        // Mis-signing money is worse than being unsure about it.
        let e = extract(&normalize("EGP 10.00 debited then credited on 01/02/2026 10:00"));
        assert_eq!(e.direction, Some(Direction::Out));
    }
}
