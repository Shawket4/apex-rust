//! Name keys: one string per way of writing a name, so that the spellings
//! of one payee meet in one key.

use crate::parser::normalize::normalize;

/// The grouping key for a counterparty: the parser's normalization (digits,
/// alef forms, ta marbuta, tatweel, diacritics, whitespace), the hamza seats,
/// lower case, and `*` read as `.` — banks mask names with either, so
/// "مينا ع*** ج***" and "مينا ع... ج..." are the same name.
pub fn fold(s: &str) -> String {
    normalize(s)
        .chars()
        .map(|c| match c {
            'ؤ' => 'و',
            'ئ' => 'ي',
            '*' => '.',
            other => other,
        })
        .flat_map(char::to_lowercase)
        .collect()
}

/// Does a masked bank name fit a full name? Banks keep each word's first
/// letter and its length: "احمد ا...... س..." fits "احمد ابراهيم سيد ...".
/// Unmasked words must match whole.
///
/// The two rarely have the same number of words. Banks cut names at a fixed
/// length, so the bank's last word may be a stub ("ابراه" for ابراهيم, "س*"
/// for سيد); and the books often keep fewer names than the bank's full legal
/// one. So the words both have are compared, at least two of them.
pub fn mask_fits(masked: &str, full: &str) -> bool {
    mask_overlap(masked, full).is_some()
}

/// How many words a fit rests on, or `None` when it doesn't fit. Two names
/// of the same length that fit word for word are `usize::MAX`: nothing left
/// unchecked.
pub fn mask_overlap(masked: &str, full: &str) -> Option<usize> {
    let masked = fold(masked);
    let full = fold(full);
    let ws: Vec<&str> = masked.split(' ').filter(|w| !w.is_empty()).collect();
    let ns: Vec<&str> = full.split(' ').filter(|w| !w.is_empty()).collect();
    let n = ws.len().min(ns.len());
    if n < 2 && !(ws.len() == 1 && ns.len() == 1) {
        return None;
    }
    // Only the bank's own last word can be a stub.
    let stub = n == ws.len();
    let fits = ws
        .iter()
        .zip(&ns)
        .enumerate()
        .all(|(i, (w, full_word))| word_fits(w, full_word, stub && i == n - 1));
    match (fits, ws.len() == ns.len()) {
        (false, _) => None,
        (true, true) => Some(usize::MAX),
        (true, false) => Some(n),
    }
}

fn word_fits(masked: &str, full: &str, may_be_cut: bool) -> bool {
    let mut chars = masked.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let rest: Vec<char> = chars.collect();
    let full_len = full.chars().count();
    if !rest.is_empty() && rest.iter().all(|&c| c == '.') {
        full.starts_with(first)
            && if may_be_cut {
                full_len >= 1 + rest.len()
            } else {
                full_len == 1 + rest.len()
            }
    } else if may_be_cut {
        full.starts_with(masked)
    } else {
        masked == full
    }
}

/// Is any word of the name masked?
pub fn is_masked(s: &str) -> bool {
    fold(s).split(' ').any(|w| {
        let mut c = w.chars();
        c.next().is_some() && {
            let rest: Vec<char> = c.collect();
            rest.len() >= 2 && rest.iter().all(|&x| x == '.')
        }
    })
}

/// Arabic script anywhere in the text. A masked Latin spelling cannot be
/// checked against an Arabic name letter by letter.
pub fn has_arabic(s: &str) -> bool {
    s.chars().any(|c| ('\u{0600}'..='\u{06FF}').contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spellings_of_one_name_share_a_key() {
        assert_eq!(fold("KAREEM  Samir"), "kareem samir");
        assert_eq!(fold("سميرة عادلى"), fold("سميره عادلي"));
        assert_eq!(fold("مينا ع*** ج***"), fold("مينا ع... ج..."));
        assert_eq!(fold("رؤوف"), "رووف");
    }

    #[test]
    fn masks_keep_first_letter_and_length() {
        assert!(mask_fits(
            "حسام ا****** ف***** س*",
            "حسام ابراهيم فاروقي سليم"
        ));
        assert!(mask_fits("مينا ع... ج... ف..", "مينا عادل جرجس فهيم"));
        // Words before the last must have the masked length exactly.
        assert!(!mask_fits("مينا ع... ج...", "مينا عادلة جرجس"));
        assert!(!mask_fits("Tamer S..... A.", "Tamer Sobhi Ali"));
        // The books keep fewer names than the bank: the shared words decide.
        assert!(mask_fits("كريم ح** ا****** ح*", "كريم حسن ابراهيم"));
        assert!(!mask_fits("كريم ح** ا****** ح*", "كريم حسين ابراهيم"));
        // One shared word is not enough.
        assert!(!mask_fits("مينا ع... ج... ف..", "مينا"));
        assert!(!mask_fits("احمد ع...", "محمود علي"));
        // The cut-off last word.
        assert!(mask_fits("Tamer S....", "Tamer Sobhi"));
        assert!(mask_fits("كريم سمير حسن عبدالر", "كريم سمير حسن عبدالرحمن"));
        assert!(!mask_fits("كريم سمير حسن عبدالر", "كريم سمير حسن احمد"));
    }

    #[test]
    fn mask_detection() {
        assert!(is_masked("Tamer S..... A........."));
        assert!(is_masked("حسام ف*** ر***"));
        assert!(!is_masked("KAREEM SAMIR HASSAN"));
        assert!(!is_masked("WATANIYA *WATANIYA STN"));
    }
}
