//! Template mining (tier 3): fingerprint a message's *shape*.
//!
//! Digits, reference codes and names are replaced with placeholders and the
//! result hashed. Two messages from the same bank template collapse to the same
//! skeleton regardless of amount, date or counterparty.
//!
//! The hash is stored on EVERY message including `ignored` ones, because
//! recurrence count is the thing that separates the two interesting cases:
//!
//!   * a skeleton seen many times with no matching template = a bank changed its
//!     SMS format. High signal; this is the alarm condition.
//!   * a skeleton seen once ever = almost certainly a human message that beat
//!     the triage gate. Low priority.
//!
//! Recon found one counter-example worth remembering: the CIB card-withdrawal
//! format appeared exactly once in seven months of history and was a real bank
//! template. Frequency is a strong prior, not proof.

use regex::Regex;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

struct Patterns {
    long_ref: Regex,
    number: Regex,
    masked: Regex,
    name_mask: Regex,
    latin_word: Regex,
    arabic_word: Regex,
    ws: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        // Reference codes first: they mix letters and digits, so replacing bare
        // numbers first would shred them into unstable fragments.
        //
        // Two alternations rather than one because Rust's regex crate has no
        // lookaround, so "at least 6 chars containing both a letter and a digit"
        // cannot be expressed directly. Each branch spells out a minimum length
        // of 6. An earlier single-branch version required 5 trailing characters
        // after the first digit, which matched `f968a807` but NOT `afb72a23` --
        // so two references from the same bank template produced different
        // skeletons.
        long_ref: Regex::new(
            r"\b[A-Za-z]+\d[A-Za-z0-9]{4,}\b|\b\d+[A-Za-z][A-Za-z0-9]{4,}\b|#[0-9a-fA-F]{6,}\b",
        )
        .unwrap(),
        // An account mask keeps its own placeholder, because which account a
        // transaction hit is structural.
        masked: Regex::new(r"\*+\d+").unwrap(),

        // Name masking is different: banks redact counterparties as `M***` and
        // `ا......`. Those runs are deleted so the surrounding letters collapse
        // into an ordinary word placeholder. Must run AFTER the account mask, or
        // `********9276` would lose its asterisks and read as a bare number.
        name_mask: Regex::new(r"[*.]{2,}").unwrap(),

        number: Regex::new(r"[\d][\d,\.\-:/]*").unwrap(),

        // Counterparty names vary per message and must not enter the skeleton.
        // Single characters count as words: an Arabic name redacted to `ا...`
        // leaves exactly one letter behind.
        latin_word: Regex::new(r"[A-Za-z]+").unwrap(),
        arabic_word: Regex::new(r"[\u{0600}-\u{06FF}]+").unwrap(),
        ws: Regex::new(r"\s+").unwrap(),
    })
}

/// Build a stable structural fingerprint from a NORMALIZED body.
///
/// Deliberately aggressive: Arabic and Latin words both collapse to `W`. The
/// skeleton is for grouping shapes, not for reading. Keeping real words would
/// make every message with a different counterparty a distinct skeleton, which
/// defeats the whole purpose.
pub fn skeletonize(normalized: &str) -> String {
    let p = patterns();
    let s = p.long_ref.replace_all(normalized, "R");
    let s = p.masked.replace_all(&s, "M");
    let s = p.name_mask.replace_all(&s, "");
    let s = p.number.replace_all(&s, "N");
    let s = p.latin_word.replace_all(&s, "W");
    let s = p.arabic_word.replace_all(&s, "W");
    let s = p.ws.replace_all(&s, " ");

    // Collapse runs of the same placeholder.
    //
    // Without this, a counterparty of "american automotive" (2 words) and one of
    // "Mohamed A********** M****** A**********" (4 words) produce different
    // skeletons for the SAME bank template -- which defeats the entire purpose,
    // since counterparty is exactly the field that varies most between messages.
    let mut out: Vec<&str> = Vec::new();
    for tok in s.split(' ').filter(|t| !t.is_empty()) {
        if out.last() == Some(&tok) && matches!(tok, "W" | "N" | "R" | "M") {
            continue;
        }
        out.push(tok);
    }

    out.join(" ")
}

/// Hex hash of the skeleton, for indexing and grouping.
pub fn skeleton_hash(normalized: &str) -> String {
    let skel = skeletonize(normalized);
    let mut h = DefaultHasher::new();
    skel.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::normalize::normalize;

    fn hash_of(raw: &str) -> String {
        skeleton_hash(&normalize(raw))
    }

    #[test]
    fn same_template_different_values_share_a_skeleton() {
        let a = "Your ABK-Egypt account *7647 was debited with EGP 500.5 on 8/7/26 17:11 for IPN transfer to Mohamed W*** E****** (ID f968a807).For queries call 19322";
        let b = "Your ABK-Egypt account *7647 was debited with EGP 9009.0 on 8/6/26 20:26 for IPN transfer to american automotive (ID afb72a23).For queries call 19322";
        assert_eq!(hash_of(a), hash_of(b));
    }

    #[test]
    fn arabic_template_groups_across_counterparties() {
        let a = "يرجى العلم أنه تم خصم مبلغ EGP 15000.00 من حساب ********9276 بتاريخ 08-08-2026 10:17 إلى fathy برقم مرجعي FT26221125YH";
        let b = "يرجى العلم أنه تم خصم مبلغ EGP 8120.00 من حساب ********5447 بتاريخ 07-08-2026 17:26 إلى وائل ا...... ا..... ا... برقم مرجعي FT2622116M45";
        assert_eq!(hash_of(a), hash_of(b));
    }

    #[test]
    fn different_templates_have_different_skeletons() {
        let abk = "Your ABK-Egypt account *7647 was debited with EGP 500.5 on 8/7/26 17:11 for IPN transfer to X (ID f968a807).For queries call 19322";
        let refbal = "Transfer reference #ac8fc63f of EGP 85.00 has been debited from your account 6001-01 through IPN on 08/08/2026 at 14:32, your available balance is 11720.54. For inquiries, call 19555.";
        assert_ne!(hash_of(abk), hash_of(refbal));
    }

    #[test]
    fn amount_and_date_do_not_affect_the_skeleton() {
        let a = "Transfer reference #ac8fc63f of EGP 85.00 has been debited from your account 6001-01 through IPN on 08/08/2026 at 14:32, your available balance is 11720.54.";
        let b = "Transfer reference #64db9715 of EGP 2000.00 has been debited from your account 6001-01 through IPN on 07/08/2026 at 22:09, your available balance is 18798.03.";
        assert_eq!(hash_of(a), hash_of(b));
    }

    #[test]
    fn hash_is_stable_across_calls() {
        let s = "Transfer reference #ac8fc63f of EGP 85.00";
        assert_eq!(hash_of(s), hash_of(s));
    }

    #[test]
    fn arabic_indic_digits_produce_the_same_skeleton() {
        let ascii = "تم خصم مبلغ EGP 15000.00 من حساب";
        let arabic = "تم خصم مبلغ EGP ١٥٠٠٠.٠٠ من حساب";
        assert_eq!(hash_of(ascii), hash_of(arabic));
    }
}
