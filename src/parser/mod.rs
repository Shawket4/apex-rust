//! Bank-SMS parsing: one path, three verdicts.
//!
//! ```text
//! message → normalize → try templates in priority order, first match wins
//!             match + PetroApp text  → Suppressed  (recognized, then excluded)
//!             match                  → Matched     (+ a transaction)
//!             no match               → Ignored
//! ```
//!
//! There is no triage scorer, no fallback field extractor, no skeleton mining
//! and no review queue. Eight templates cover every bank SMS ever received in
//! this chat (100/100, measured against the full corpus on 2026-08-12); a
//! message they miss is visible in the Messages screen, and the fix is a new
//! template row — not a lower tier of guessing.

pub mod normalize;
pub mod templates;

use rust_decimal::Decimal;

pub const STATUS_MATCHED: &str = "matched";
pub const STATUS_IGNORED: &str = "ignored";
pub const STATUS_SUPPRESSED: &str = "suppressed";

/// What the parser concluded about one message.
#[derive(Debug, Clone)]
pub enum Verdict {
    /// A template matched and every required field extracted. Becomes a
    /// transaction.
    Matched { template: String, fields: Extracted },
    /// A template matched but the text mentions PetroApp: that fuel already
    /// arrives as `public.fuel_events` via the PetroApp sync, and recording the
    /// bank's notification too would count the same money twice. Recognized,
    /// then excluded — a decision, not a gap.
    Suppressed { template: String },
    /// No template matched: chatter, media, or a genuinely new bank format.
    /// New formats are found by reading the Messages screen, not by a scorer.
    Ignored,
}

impl Verdict {
    pub fn status(&self) -> &'static str {
        match self {
            Verdict::Matched { .. } => STATUS_MATCHED,
            Verdict::Suppressed { .. } => STATUS_SUPPRESSED,
            Verdict::Ignored => STATUS_IGNORED,
        }
    }

    pub fn template(&self) -> Option<&str> {
        match self {
            Verdict::Matched { template, .. } | Verdict::Suppressed { template } => Some(template),
            Verdict::Ignored => None,
        }
    }
}

/// The fields a matched template produced. `direction`, `amount` and
/// `occurred_at` are guaranteed present — a match that cannot produce all
/// three is downgraded to Ignored with a loud log (see `parse`).
#[derive(Debug, Clone)]
pub struct Extracted {
    pub direction: String, // "in" | "out"
    pub amount: Decimal,
    pub currency: String,
    pub occurred_at: chrono::DateTime<chrono::Utc>,
    pub account: Option<String>,
    pub counterparty: Option<String>,
    pub reference: Option<String>,
}

/// PetroApp detector, applied AFTER a template matches (on the normalized
/// body). The بتروآب spelling folds to بترو اب under normalization, so the
/// second alternation covers it.
pub fn petroapp_pattern() -> &'static regex::Regex {
    static P: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    P.get_or_init(|| regex::Regex::new(r"(?i)petroapp|بترو\s*اب").unwrap())
}

/// Parse one message body. `reference` is the message's arrival time — it
/// supplies the year for the one date format that omits it.
pub fn parse(
    body: &str,
    compiled: &[templates::CompiledTemplate],
    reference: Option<chrono::DateTime<chrono::Utc>>,
) -> Verdict {
    let normalized = normalize::normalize(body);
    if normalized.is_empty() {
        return Verdict::Ignored; // media message
    }

    for t in compiled {
        let Some(caps) = t.regex.captures(&normalized) else {
            continue;
        };

        if petroapp_pattern().is_match(&normalized) {
            return Verdict::Suppressed {
                template: t.name.clone(),
            };
        }

        match templates::apply(t, &caps, reference) {
            Some(fields) => {
                return Verdict::Matched {
                    template: t.name.clone(),
                    fields,
                }
            }
            None => {
                // A matched pattern that cannot produce direction+amount+date
                // should be impossible: template writes validate their own
                // sample end-to-end. If it happens anyway, refuse to guess and
                // make it impossible to miss.
                log::error!(
                    "template '{}' matched but extraction was incomplete — message ignored; \
                     fix the template (its sample validation has a gap)",
                    t.name
                );
                return Verdict::Ignored;
            }
        }
    }

    Verdict::Ignored
}

/// A 0.1% fee applies to IPN transfers. Some banks report a fee-INCLUSIVE
/// amount (2002.00 = 2000 principal + 2.00 fee); others report the clean
/// amount and take the fee separately, visible only as balance drift.
///
/// Deliberately DERIVED at read time, never stored: which convention a given
/// message uses cannot be known reliably, and storing a guess would make it
/// indistinguishable from a fact. Both conventions occur in the same chat.
pub fn derive_fee(amount: Decimal) -> (Decimal, Decimal) {
    use std::str::FromStr;

    let divisor = Decimal::from_str("1.001").unwrap();
    let implied_principal = (amount / divisor).round_dp(2);
    let reconstructed = (implied_principal * divisor).round_dp(2);

    if reconstructed == amount && implied_principal.fract().is_zero() {
        (implied_principal, amount - implied_principal)
    } else {
        (amount, Decimal::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn fee_inclusive_amounts_split() {
        assert_eq!(derive_fee(dec!(2002.00)), (dec!(2000), dec!(2.00)));
        assert_eq!(derive_fee(dec!(500.5)), (dec!(500), dec!(0.5)));
        assert_eq!(derive_fee(dec!(9009.0)), (dec!(9000), dec!(9.0)));
    }

    #[test]
    fn clean_amounts_do_not_split() {
        assert_eq!(derive_fee(dec!(85.00)), (dec!(85.00), dec!(0)));
        assert_eq!(derive_fee(dec!(15000.00)), (dec!(15000.00), dec!(0)));
        // 135.72 is not principal*1.001 for any round principal
        assert_eq!(derive_fee(dec!(135.72)), (dec!(135.72), dec!(0)));
    }

    #[test]
    fn petroapp_detector_covers_folded_arabic() {
        // بتروآب normalizes to بترواب; the pattern must catch both spellings.
        assert!(petroapp_pattern().is_match("petroapp c**"));
        assert!(petroapp_pattern().is_match("PetroApp"));
        assert!(petroapp_pattern().is_match(&normalize::normalize("بتروآب")));
        assert!(petroapp_pattern().is_match("بترو اب"));
        assert!(!petroapp_pattern().is_match("تحويل الي احمد"));
    }
}
