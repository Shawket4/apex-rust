//! Normalization pre-pass.
//!
//! Runs on a COPY of the body; `raw_messages.body` always keeps the original.
//!
//! Every template pattern in the registry is written against the output of this
//! function. A pattern that fails on `١٥٠٠٠` but succeeds on `15000` is a
//! normalization bug, not a pattern gap -- and a pattern containing a raw `إلى`
//! can never match text this function has rewritten to `الي`.

/// Normalize a message body for matching.
///
/// No NFKC pass: the folds this parser depends on (digits, alef forms, ta
/// marbuta, tatweel, bidi marks) are all explicit below, and compatibility
/// forms have never appeared in the corpus. The previous implementation
/// carried an NFKC step that was a do-nothing stub — deleted rather than
/// pretended.
pub fn normalize(input: &str) -> String {
    let mut s = String::with_capacity(input.len());

    for ch in input.chars() {
        match ch {
            // Arabic-Indic and Persian digits -> ASCII.
            '\u{0660}'..='\u{0669}' => {
                s.push((b'0' + (ch as u32 - 0x0660) as u8) as char);
            }
            '\u{06F0}'..='\u{06F9}' => {
                s.push((b'0' + (ch as u32 - 0x06F0) as u8) as char);
            }

            // Bidi control marks. Invisible, and they sit right between the
            // number and the currency code in real bank SMS.
            '\u{200E}'
            | '\u{200F}'
            | '\u{061C}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}' => {}

            // Zero-width characters.
            '\u{200B}'..='\u{200D}' | '\u{FEFF}' => {}

            // Tatweel (kashida) is pure decoration: بـ and ب are the same word.
            '\u{0640}' => {}

            // Arabic diacritics.
            '\u{064B}'..='\u{0652}' | '\u{0653}'..='\u{0655}' | '\u{0670}' => {}

            // Alef forms unified.
            '\u{0623}' | '\u{0625}' | '\u{0622}' | '\u{0671}' => s.push('\u{0627}'),
            // Alef maqsura -> ya.
            '\u{0649}' => s.push('\u{064A}'),
            // Ta marbuta -> ha.
            '\u{0629}' => s.push('\u{0647}'),

            // Arabic decimal separator and thousands mark.
            '\u{066B}' => s.push('.'),
            '\u{066C}' => s.push(','),

            // NBSP and friends collapse into ordinary spaces below.
            '\u{00A0}' | '\u{202F}' | '\u{2007}' => s.push(' '),

            other => s.push(other),
        }
    }

    // Collapse whitespace runs and trim. Real messages contain double spaces
    // around amounts ("مبلغ  EGP 8000.00  من"), which would otherwise force
    // every pattern to guess at spacing.
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !last_was_space {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.push(ch);
            last_was_space = false;
        }
    }

    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_arabic_indic_digits() {
        assert_eq!(normalize("١٥٠٠٠"), "15000");
        assert_eq!(normalize("مبلغ ٢٠٢٠٠.٥٠"), "مبلغ 20200.50");
    }

    #[test]
    fn folds_persian_digits() {
        assert_eq!(normalize("۱۲۳۴۵۶۷۸۹۰"), "1234567890");
    }

    #[test]
    fn strips_bidi_and_zero_width_marks() {
        // A real hazard: an invisible RLM between the amount and the currency.
        assert_eq!(normalize("EGP\u{200F} 1500.00"), "EGP 1500.00");
        assert_eq!(normalize("15\u{200B}00"), "1500");
    }

    #[test]
    fn strips_tatweel_so_ba_matches() {
        // "المنتهية بـ **2234" -> "المنتهيه ب **2234"
        assert_eq!(normalize("بـ **2234"), "ب **2234");
    }

    #[test]
    fn unifies_alef_forms() {
        assert_eq!(normalize("أحمد"), "احمد");
        assert_eq!(normalize("إلى"), "الي");
        assert_eq!(normalize("آسف"), "اسف");
    }

    #[test]
    fn maps_ta_marbuta_and_alef_maqsura() {
        assert_eq!(normalize("بطاقة"), "بطاقه");
        assert_eq!(normalize("يرجى"), "يرجي");
    }

    #[test]
    fn strips_diacritics() {
        assert_eq!(normalize("مَبْلَغ"), "مبلغ");
    }

    #[test]
    fn collapses_whitespace_runs() {
        assert_eq!(normalize("مبلغ  EGP 8000.00  من"), "مبلغ EGP 8000.00 من");
        assert_eq!(normalize("  a \n\t b  "), "a b");
    }

    #[test]
    fn arabic_decimal_and_thousands_marks() {
        assert_eq!(normalize("1٬500٫25"), "1,500.25");
    }

    #[test]
    fn leaves_plain_english_untouched() {
        let s = "Transfer reference #ac8fc63f of EGP 85.00 has been debited";
        assert_eq!(normalize(s), s);
    }

    /// The exact failure mode the spec calls out.
    #[test]
    fn arabic_indic_amount_normalizes_to_matchable_form() {
        let raw = "تم خصم مبلغ EGP ١٥٠٠٠.٠٠ من حساب";
        assert!(normalize(raw).contains("15000.00"));
    }
}
