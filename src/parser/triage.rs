//! Triage gate: is this message a bank SMS at all?
//!
//! This is load-bearing, not defence in depth. Recon established that the target
//! chat is a 1:1 DM in which the SAME sender JID, with `is_from_me = false`,
//! sends both bank SMS and ordinary conversation, and that the SMS carry no
//! forwarder prefix. Both of the "stronger discriminators" the design hoped for
//! are unavailable, so weighted content scoring is the only mechanism.
//!
//! Getting this wrong is expensive in both directions: a bank SMS scored too low
//! is silently lost, and chatter scored too high fills the review queue with
//! noise until nobody looks at it. Roughly 93% of text messages in this chat are
//! ordinary conversation.

use regex::Regex;
use std::sync::OnceLock;

/// Score at or above which a message enters the extraction pipeline.
pub const DEFAULT_THRESHOLD: i32 = 40;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Below threshold: ordinary chatter. Expected, not an error, no review entry.
    Ignore,
    /// At or above threshold: run the extraction pipeline.
    Extract,
}

#[derive(Debug, Clone)]
pub struct Triage {
    pub score: i32,
    pub route: Route,
    /// Which signals fired, for `/raw/ignored` auditing and threshold tuning.
    pub signals: Vec<&'static str>,
}

struct Patterns {
    hotline: Regex,
    currency_adjacent_number: Regex,
    masked_account: Regex,
    bank_reference: Regex,
    txn_keyword: Regex,
    date: Regex,
    time: Regex,
    emoji: Regex,
    conversational: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        // Egyptian bank hotlines observed in the corpus, plus common others.
        hotline: Regex::new(r"\b(19322|19555|19666|19623|19888|16664|19011|19100)\b").unwrap(),

        // An ISO currency code adjacent to a number. The single strongest signal:
        // humans write "1500 جنيه", banks write "EGP 1500.00".
        currency_adjacent_number: Regex::new(r"\b[A-Z]{3}\s*[\d,]+(?:\.\d{1,2})?\b").unwrap(),

        // *7647 | ********9276 | **2234 | 6001-01
        masked_account: Regex::new(r"(\*{1,10}\d{3,6}\b)|(\b\d{4}-\d{2}\b)").unwrap(),

        // FT26221D24WS, #ac8fc63f, (ID f968a807)
        bank_reference: Regex::new(r"\bFT[A-Z0-9]{6,}\b|#[0-9a-f]{6,}\b|\(ID\s+\w+\)").unwrap(),

        // Note `خصم` is deliberately here at MEDIUM weight, not high: it appears
        // in ordinary messages such as "تعديل لخصم قطس كاوتش".
        txn_keyword: Regex::new(
            r"(?i)\b(debited|credited|withdrawn|balance|transfer|IPN)\b|خصم|اضافه|ايداع|سحب|رصيد|حساب|مبلغ|مرجعي",
        )
        .unwrap(),

        date: Regex::new(r"\b\d{1,2}[/-]\d{1,2}[/-]\d{2,4}\b").unwrap(),
        time: Regex::new(r"\b\d{1,2}:\d{2}\b").unwrap(),

        // Negative signals. Banks do not send emoji or ask questions.
        emoji: Regex::new(r"[\x{1F300}-\x{1FAFF}\x{2600}-\x{27BF}\x{FE0F}]").unwrap(),

        // Second-person and conversational forms, Arabic + Franco-Arabic + English.
        conversational: Regex::new(
            r"(?i)\?|؟|\b(you|please|thanks|ok|okay|yes|no|why|what|how)\b|\b(ana|enta|mashe|khalas|3ady|tamam|meen|feen|ezay)\b|انت|ايه|ليه|ازاي|تمام|خلاص|ممكن|عايز|عاوز|لو سمحت|شكرا",
        )
        .unwrap(),
    })
}

/// Score a NORMALIZED message body.
///
/// Weights are additive; the negative signals can and should drive a chatty
/// message below the threshold even when it mentions money.
pub fn score(normalized: &str, threshold: i32) -> Triage {
    let p = patterns();
    let mut score = 0;
    let mut signals = Vec::new();

    // An empty body is a media message. Not a bank SMS, not an error.
    if normalized.trim().is_empty() {
        return Triage {
            score: 0,
            route: Route::Ignore,
            signals: vec!["empty_body"],
        };
    }

    // --- high-weight signals ------------------------------------------------
    if p.hotline.is_match(normalized) {
        score += 40;
        signals.push("hotline");
    }
    if p.currency_adjacent_number.is_match(normalized) {
        score += 35;
        signals.push("currency_adjacent_number");
    }
    if p.masked_account.is_match(normalized) {
        score += 30;
        signals.push("masked_account");
    }
    if p.bank_reference.is_match(normalized) {
        score += 30;
        signals.push("bank_reference");
    }

    // --- medium-weight signals ----------------------------------------------
    if p.txn_keyword.is_match(normalized) {
        score += 15;
        signals.push("txn_keyword");
    }
    // Date AND time together, not either alone: humans write one, banks write both.
    if p.date.is_match(normalized) && p.time.is_match(normalized) {
        score += 15;
        signals.push("date_and_time");
    }

    // --- low-weight signal --------------------------------------------------
    let len = normalized.chars().count();
    if (60..=400).contains(&len) {
        score += 5;
        signals.push("sms_length");
    }

    // --- negative signals ---------------------------------------------------
    if p.emoji.is_match(normalized) {
        score -= 40;
        signals.push("emoji");
    }
    if p.conversational.is_match(normalized) {
        score -= 35;
        signals.push("conversational");
    }
    // Very short messages are conversation, whatever else they contain.
    if len < 25 {
        score -= 25;
        signals.push("too_short");
    }

    let route = if score >= threshold {
        Route::Extract
    } else {
        Route::Ignore
    };

    Triage {
        score,
        route,
        signals,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::normalize::normalize;

    fn route_of(raw: &str) -> Route {
        score(&normalize(raw), DEFAULT_THRESHOLD).route
    }

    #[test]
    fn real_bank_sms_route_to_extract() {
        assert_eq!(route_of("Your ABK-Egypt account *7647 was debited with EGP 500.5 on 8/7/26 17:11 for IPN transfer to Mohamed W*** E****** E******* (ID f968a807).For queries call 19322"), Route::Extract);
        assert_eq!(route_of("Transfer reference #ac8fc63f of EGP 85.00 has been debited from your account 6001-01 through IPN on 08/08/2026 at 14:32, your available balance is 11720.54. For inquiries, call 19555."), Route::Extract);
        assert_eq!(route_of("يرجى العلم أنه تم خصم مبلغ EGP 15000.00 من حساب ********9276 عبر شبكة المدفوعات اللحظية (IPN) من خلال خدمات الإنترنت البنكية بتاريخ 08-08-2026 10:17 إلى fathy برقم مرجعي FT26221125YH للمزيد، يرجي الاتصال بـ 19666."), Route::Extract);
        assert_eq!(route_of("تم سحب مبلغ  EGP 8000.00  من بطاقة الخصم المباشر المنتهية بـ **2234 من CIB EL NASRBR 2 BNA في 08/08/26 10:32 ، الرصيد المتاح EGP 8579.76"), Route::Extract);
    }

    #[test]
    fn ordinary_chatter_routes_to_ignore() {
        assert_eq!(route_of("Mashe 5alas 3ady"), Route::Ignore);
        assert_eq!(route_of("هو لازم contact"), Route::Ignore);
        assert_eq!(route_of("لا"), Route::Ignore);
        assert_eq!(route_of("لو بعتلك الرسايل كدا"), Route::Ignore);
        assert_eq!(route_of("في طريقة تتسجل اوتوماتيك في السيستم"), Route::Ignore);
        assert_eq!(route_of("Shady t3raf el messages dyh tygy 3ala group bdal private?"), Route::Ignore);
    }

    /// The exact false positive found in the corpus: an ordinary message
    /// containing `خصم`, a medium-weight transaction keyword.
    #[test]
    fn human_message_containing_debit_keyword_is_ignored() {
        assert_eq!(route_of("تعديل لخصم قطس كاوتش"), Route::Ignore);
    }

    /// A human talking about money with a date must still be ignored: this is the
    /// case that a naive keyword gate gets wrong.
    #[test]
    fn human_message_mentioning_amount_and_date_is_ignored() {
        assert_eq!(route_of("ابعتلي 5000 جنيه بكرة 8/8 لو سمحت"), Route::Ignore);
        assert_eq!(route_of("did you send the 2000 yesterday? thanks"), Route::Ignore);
    }

    #[test]
    fn media_messages_are_ignored_not_errored() {
        let t = score("", DEFAULT_THRESHOLD);
        assert_eq!(t.route, Route::Ignore);
        assert!(t.signals.contains(&"empty_body"));
    }

    #[test]
    fn arabic_indic_digits_do_not_break_scoring() {
        // Same message, Arabic-Indic numerals. Must score identically once
        // normalized -- otherwise it is a normalization bug, not a pattern gap.
        let ascii = normalize("تم خصم مبلغ EGP 15000.00 من حساب ********9276 بتاريخ 08-08-2026 10:17 برقم مرجعي FT26221125YH يرجي الاتصال بـ 19666");
        let arabic = normalize("تم خصم مبلغ EGP ١٥٠٠٠.٠٠ من حساب ********٩٢٧٦ بتاريخ ٠٨-٠٨-٢٠٢٦ ١٠:١٧ برقم مرجعي FT26221125YH يرجي الاتصال بـ ١٩٦٦٦");
        assert_eq!(
            score(&ascii, DEFAULT_THRESHOLD).score,
            score(&arabic, DEFAULT_THRESHOLD).score
        );
    }
}
