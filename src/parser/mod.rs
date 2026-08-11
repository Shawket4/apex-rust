//! Bank-SMS parser.
//!
//! Pipeline, in order:
//!   1. normalize   -- on a copy; `raw_messages.body` keeps the original
//!   2. skeleton    -- fingerprint stored on EVERY message, even ignored ones
//!   3. noise check -- skeletons a human marked as chatter auto-ignore
//!   4. triage      -- is this a bank SMS at all?
//!   5. tier 1      -- template registry, deterministic, confidence 100
//!   6. tier 2      -- field extractors, confidence < 100, queued for review
//!
//! Nothing is ever dropped: every message ends in exactly one terminal status.

#[cfg(test)]
mod fixture_tests;

pub mod extractors;
pub mod normalize;
pub mod skeleton;
pub mod templates;
pub mod triage;
pub mod worker;

use rust_decimal::Decimal;
use std::str::FromStr;

use crate::errors::AppResult;
use chrono::{DateTime, Utc};

pub use templates::PARSER_VERSION;

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type, serde::Serialize, serde::Deserialize)]
#[sqlx(type_name = "direction", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    In,
    Out,
}

impl FromStr for Direction {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "in" => Ok(Direction::In),
            "out" => Ok(Direction::Out),
            _ => Err(()),
        }
    }
}

impl Direction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Direction::In => "in",
            Direction::Out => "out",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseStatus {
    Parsed,
    Partial,
    Unmatched,
    Ignored,
}

impl ParseStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ParseStatus::Parsed => "parsed",
            ParseStatus::Partial => "partial",
            ParseStatus::Unmatched => "unmatched",
            ParseStatus::Ignored => "ignored",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseMethod {
    Template,
    Extractors,
}

impl ParseMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            ParseMethod::Template => "template",
            ParseMethod::Extractors => "extractors",
        }
    }
}

/// Everything the parser concluded about one message.
#[derive(Debug, Clone)]
pub struct ParseOutcome {
    pub status: ParseStatus,
    pub skeleton_hash: String,
    pub triage_score: i16,
    pub triage_signals: Vec<&'static str>,
    pub parser_version: i32,

    pub method: Option<ParseMethod>,
    pub confidence: i16,
    pub template: Option<String>,

    pub direction: Option<Direction>,
    pub amount: Option<Decimal>,
    pub currency: Option<String>,
    pub account: Option<String>,
    pub counterparty: Option<String>,
    pub reference: Option<String>,
    pub balance_after: Option<Decimal>,
    pub occurred_at: Option<DateTime<Utc>>,
}

impl ParseOutcome {
    /// Should this produce a row in `transactions`?
    ///
    /// `partial` rows do: they carry a real amount and date and are worth money,
    /// they are simply flagged for review. `unmatched` and `ignored` do not.
    pub fn yields_transaction(&self) -> bool {
        matches!(self.status, ParseStatus::Parsed | ParseStatus::Partial)
            && self.amount.is_some()
            && self.occurred_at.is_some()
    }

    fn ignored(skeleton_hash: String, triage: &triage::Triage) -> Self {
        Self {
            status: ParseStatus::Ignored,
            skeleton_hash,
            triage_score: triage.score.clamp(0, 100) as i16,
            triage_signals: triage.signals.clone(),
            parser_version: PARSER_VERSION,
            method: None,
            confidence: 0,
            template: None,
            direction: None,
            amount: None,
            currency: None,
            account: None,
            counterparty: None,
            reference: None,
            balance_after: None,
            occurred_at: None,
        }
    }
}

/// Parse one raw message body.
///
/// `noise_skeletons` is the set of skeleton hashes a human has confirmed are
/// chatter; anything matching auto-ignores. That is what stops the review queue
/// refilling with the same false positive after every reparse.
/// Messages about PetroApp are excluded from the ledger entirely.
///
/// Fuel bought through PetroApp already reaches the system as `fuel_events` via
/// the PetroApp sync. Recording the bank's transfer notification as well would
/// count the same money twice -- once as a fuel event and once as a bank
/// transaction -- and silently inflate every fuel and total figure.
///
/// Applied AFTER the templates run, not instead of them, so the format is still
/// recognised: an unrecognised message is a gap to investigate, whereas a
/// suppressed one is a decision. The distinction matters when a bank changes its
/// wording.
fn petroapp_pattern() -> &'static regex::Regex {
    static P: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    P.get_or_init(|| regex::Regex::new(r"(?i)petroapp|بترو\s*اب|بتروآب").unwrap())
}

pub fn parse(
    body: &str,
    compiled: &[templates::CompiledTemplate],
    noise_skeletons: &std::collections::HashSet<String>,
    threshold: i32,
    // `reference` is when the message arrived; it supplies the year for date
    // formats that omit one.
    reference: Option<chrono::DateTime<chrono::Utc>>,
) -> ParseOutcome {
    let normalized = normalize::normalize(body);
    let skel = skeleton::skeleton_hash(&normalized);

    // Human-confirmed noise short-circuits everything below, including triage.
    if noise_skeletons.contains(&skel) {
        let t = triage::Triage {
            score: 0,
            route: triage::Route::Ignore,
            signals: vec!["known_noise_skeleton"],
        };
        return ParseOutcome::ignored(skel, &t);
    }

    let t = triage::score(&normalized, threshold);
    if t.route == triage::Route::Ignore {
        return ParseOutcome::ignored(skel, &t);
    }

    // PetroApp transfers are already counted as fuel events; see above.
    if petroapp_pattern().is_match(&normalized) {
        let suppressed = triage::Triage {
            score: t.score,
            route: triage::Route::Ignore,
            signals: {
                let mut sig = t.signals.clone();
                sig.push("petroapp_suppressed");
                sig
            },
        };
        return ParseOutcome::ignored(skel, &suppressed);
    }

    let triage_score = t.score.clamp(0, 100) as i16;

    // --- tier 1: template registry -----------------------------------------
    if let Some(m) = templates::match_first(compiled, &normalized, reference) {
        // A template match is deterministic. Some formats genuinely lack fields
        // (cib_card has no reference number at all), so a missing optional field
        // does not reduce confidence.
        return ParseOutcome {
            status: ParseStatus::Parsed,
            skeleton_hash: skel,
            triage_score,
            triage_signals: t.signals,
            parser_version: PARSER_VERSION,
            method: Some(ParseMethod::Template),
            confidence: 100,
            template: Some(m.template_name),
            direction: m.direction,
            amount: m.amount,
            currency: m.currency,
            account: m.account,
            counterparty: m.counterparty,
            reference: m.reference,
            balance_after: m.balance_after,
            occurred_at: m.occurred_at,
        };
    }

    // --- tier 2: field extractors ------------------------------------------
    let e = extractors::extract(&normalized, reference);
    let status = if e.is_viable() {
        ParseStatus::Partial
    } else {
        ParseStatus::Unmatched
    };

    ParseOutcome {
        status,
        skeleton_hash: skel,
        triage_score,
        triage_signals: t.signals,
        parser_version: PARSER_VERSION,
        method: Some(ParseMethod::Extractors),
        confidence: e.confidence(),
        template: None,
        direction: e.direction,
        amount: e.amount,
        currency: e.currency,
        account: e.account,
        counterparty: e.counterparty,
        reference: e.reference,
        balance_after: e.balance_after,
        occurred_at: e.occurred_at,
    }
}

/// Load the set of skeletons humans have marked as noise.
pub async fn load_noise_skeletons(
    pool: &sqlx::PgPool,
) -> AppResult<std::collections::HashSet<String>> {
    let rows = sqlx::query!("SELECT skeleton_hash FROM banksms.noise_skeletons")
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|r| r.skeleton_hash).collect())
}

/// A 0.1% fee applies to IPN transfers. Some banks report a fee-INCLUSIVE amount
/// (2002.00 = 2000 principal + 2.00 fee); others report the clean amount and take
/// the fee separately, visible only as balance drift.
///
/// This is deliberately a DERIVED calculation, never a stored column: which
/// convention a given message uses cannot be known reliably, and storing a guess
/// would make it indistinguishable from a fact. Both conventions occur in the
/// same chat.
pub fn derive_fee(amount: Decimal) -> (Decimal, Decimal) {
    use rust_decimal::prelude::*;

    // If amount == principal * 1.001 for a round principal, treat it as inclusive.
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
    fn fee_inclusive_amounts_are_decomposed() {
        // Real observed amounts: principal + 0.1%.
        assert_eq!(derive_fee(dec!(2002.00)), (dec!(2000), dec!(2.00)));
        assert_eq!(derive_fee(dec!(1001.0)), (dec!(1000), dec!(1.0)));
        assert_eq!(derive_fee(dec!(8008.0)), (dec!(8000), dec!(8.0)));
        assert_eq!(derive_fee(dec!(9009.0)), (dec!(9000), dec!(9.0)));
    }

    #[test]
    fn clean_amounts_report_no_fee() {
        // Equally real: these banks deduct the fee separately.
        assert_eq!(derive_fee(dec!(85.00)), (dec!(85.00), Decimal::ZERO));
        assert_eq!(derive_fee(dec!(15000.00)), (dec!(15000.00), Decimal::ZERO));
        assert_eq!(derive_fee(dec!(4985.00)), (dec!(4985.00), Decimal::ZERO));
    }

    #[test]
    fn direction_round_trips_through_strings() {
        assert_eq!(Direction::from_str("out").unwrap(), Direction::Out);
        assert_eq!(Direction::from_str("IN").unwrap(), Direction::In);
        assert!(Direction::from_str("sideways").is_err());
        assert_eq!(Direction::Out.as_str(), "out");
    }
}
