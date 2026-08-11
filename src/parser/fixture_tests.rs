//! Acceptance tests for the parser, run against fixtures cut from the real chat
//! history (479 text messages, 2026-01-13 .. 2026-08-08).
//!
//! The templates under test are read from the shipped seed migration rather than
//! re-declared here. That is deliberate: re-declaring them would let the code and
//! the data that actually ships drift apart while every test stayed green.

use std::collections::{HashMap, HashSet};

use super::*;
use crate::parser::templates::CompiledTemplate;

/// Templates are seeded across MULTIPLE migrations: the original four, plus any
/// added later. An applied migration's checksum is immutable, so a new template
/// arrives in a new file rather than by editing the seed -- which means this
/// test has to read every file that seeds one, not just the first.
const SEED_SQL: &str = concat!(
    include_str!("../../migrations/20260808120100_seed_parse_templates.up.sql"),
    include_str!("../../migrations/20260809180000_instant_transfer_template.up.sql"),
    include_str!("../../migrations/20260810080000_wallet_transfer_template.up.sql"),
);
const BANK_SMS: &str = include_str!("../../tests/fixtures/bank_sms.json");
const CHATTER: &str = include_str!("../../tests/fixtures/chatter.json");
/// Messages that a template DOES recognise but which are deliberately excluded
/// from the ledger -- currently PetroApp transfers, which already arrive as fuel
/// events. They live apart from `bank_sms.json` because that set asserts every
/// message becomes a transaction, and these must not.
const SUPPRESSED: &str = include_str!("../../tests/fixtures/suppressed.json");

#[derive(serde::Deserialize)]
struct Fixture {
    wa_message_id: String,
    wa_timestamp: String,
    body: String,
    #[serde(default)]
    expect_template: Option<String>,
}

/// Pull the four seeded templates straight out of the migration file.
fn seeded_templates() -> Vec<CompiledTemplate> {
    let mut out = Vec::new();

    // Each row is: ('name', $re$pattern$re$, '[formats]'::jsonb, '{map}'::jsonb, priority,
    for (idx, chunk) in SEED_SQL.split("$re$").enumerate().collect::<Vec<_>>().chunks(2).enumerate()
    {
        let _ = idx;
        if chunk.len() < 2 {
            continue;
        }
        let (before, pattern) = (chunk[0].1, chunk[1].1);

        // Name is the last single-quoted literal before the pattern.
        let name = before
            .rsplit('\'')
            .nth(1)
            .unwrap_or_default()
            .to_string();

        // Formats and direction map follow the pattern.
        let after = SEED_SQL
            .split_once(pattern)
            .map(|(_, a)| a)
            .unwrap_or_default();
        let date_formats: Vec<String> = extract_json_array(after);
        let direction_map: HashMap<String, Direction> = extract_direction_map(after);

        if name.is_empty() || pattern.trim().is_empty() {
            continue;
        }

        out.push(CompiledTemplate {
            id: out.len() as i64,
            name,
            regex: regex::Regex::new(pattern)
                .unwrap_or_else(|e| panic!("seeded pattern does not compile: {e}")),
            date_formats,
            direction_map,
            priority: out.len() as i32,
        });
    }

    assert_eq!(
        out.len(),
        6,
        "expected 6 seeded templates, parsed {} from the migrations",
        out.len()
    );
    out
}

fn extract_json_array(s: &str) -> Vec<String> {
    let start = match s.find('[') {
        Some(i) => i,
        None => return vec![],
    };
    let end = match s[start..].find(']') {
        Some(i) => start + i + 1,
        None => return vec![],
    };
    serde_json::from_str(&s[start..end]).unwrap_or_default()
}

fn extract_direction_map(s: &str) -> HashMap<String, Direction> {
    let start = match s.find('{') {
        Some(i) => i,
        None => return HashMap::new(),
    };
    let end = match s[start..].find('}') {
        Some(i) => start + i + 1,
        None => return HashMap::new(),
    };
    let raw: HashMap<String, String> = serde_json::from_str(&s[start..end]).unwrap_or_default();
    raw.into_iter()
        .filter_map(|(k, v)| v.parse::<Direction>().ok().map(|d| (k, d)))
        .collect()
}

fn no_noise() -> HashSet<String> {
    HashSet::new()
}

/// Every seeded pattern must match at least one real message.
///
/// This is the guard against the single easiest way to break this parser: writing
/// a pattern in raw form when the matcher sees normalized text. A pattern
/// containing `إلى` compiles fine, inserts fine, and silently never matches.
#[test]
fn every_seeded_template_matches_real_traffic() {
    let templates = seeded_templates();
    // Suppressed formats count as real traffic: the point of this test is that a
    // pattern matches the wording banks actually send, not that it produces a
    // transaction.
    let mut fixtures: Vec<Fixture> = serde_json::from_str(BANK_SMS).unwrap();
    fixtures.extend(serde_json::from_str::<Vec<Fixture>>(SUPPRESSED).unwrap());

    for t in &templates {
        let hits = fixtures
            .iter()
            .filter(|f| {
                t.regex.is_match(&normalize::normalize(&f.body))
            })
            .count();
        assert!(
            hits > 0,
            "template '{}' matched no real message -- almost certainly written in \
             pre-normalization form",
            t.name
        );
    }
}

/// The headline acceptance criterion: every bank SMS in the corpus parses via
/// tier 1, with the expected template.
#[test]
fn all_bank_sms_parse_via_tier_one() {
    let templates = seeded_templates();
    let fixtures: Vec<Fixture> = serde_json::from_str(BANK_SMS).unwrap();
    assert_eq!(fixtures.len(), 35, "fixture set changed size");

    let mut failures = Vec::new();

    for f in &fixtures {
        // Pass the fixture's own arrival time: formats that omit the year
        // resolve against it, exactly as they do in production.
        let outcome = parse(
            &f.body,
            &templates,
            &no_noise(),
            triage::DEFAULT_THRESHOLD,
            f.wa_timestamp.parse::<chrono::DateTime<chrono::Utc>>().ok(),
        );

        if outcome.status != ParseStatus::Parsed {
            failures.push(format!(
                "{}: status {:?} (triage {}) -- {}",
                f.wa_message_id,
                outcome.status,
                outcome.triage_score,
                f.body.chars().take(70).collect::<String>()
            ));
            continue;
        }
        if outcome.method != Some(ParseMethod::Template) {
            failures.push(format!("{}: not a tier-1 match", f.wa_message_id));
            continue;
        }
        if outcome.confidence != 100 {
            failures.push(format!(
                "{}: confidence {} on a template match",
                f.wa_message_id, outcome.confidence
            ));
        }
        if let Some(expected) = &f.expect_template {
            assert_eq!(
                outcome.template.as_deref(),
                Some(expected.as_str()),
                "{}: matched the wrong template",
                f.wa_message_id
            );
        }
        // Money and date are the minimum viable row.
        assert!(outcome.amount.is_some(), "{}: no amount", f.wa_message_id);
        assert!(
            outcome.occurred_at.is_some(),
            "{}: no date",
            f.wa_message_id
        );
        assert!(
            outcome.direction.is_some(),
            "{}: no direction",
            f.wa_message_id
        );
    }

    assert!(
        failures.is_empty(),
        "{} of {} bank SMS failed tier 1:\n{}",
        failures.len(),
        fixtures.len(),
        failures.join("\n")
    );
}

/// The other half of the acceptance criterion, and the one that actually protects
/// the review queue: ordinary conversation must route to `ignored` with ZERO
/// review-queue entries.
#[test]
fn all_chatter_routes_to_ignored() {
    let templates = seeded_templates();
    let fixtures: Vec<Fixture> = serde_json::from_str(CHATTER).unwrap();

    let mut leaked = Vec::new();
    for f in &fixtures {
        // Pass the fixture's own arrival time: formats that omit the year
        // resolve against it, exactly as they do in production.
        let outcome = parse(
            &f.body,
            &templates,
            &no_noise(),
            triage::DEFAULT_THRESHOLD,
            f.wa_timestamp.parse::<chrono::DateTime<chrono::Utc>>().ok(),
        );
        if outcome.status != ParseStatus::Ignored {
            leaked.push(format!(
                "[{:?} score={}] {}",
                outcome.status,
                outcome.triage_score,
                f.body.chars().take(80).collect::<String>()
            ));
        }
    }

    assert!(
        leaked.is_empty(),
        "{} of {} chatter messages leaked past the triage gate:\n{}",
        leaked.len(),
        fixtures.len(),
        leaked.join("\n")
    );
}

/// Parsed timestamps must agree with the WhatsApp envelope after Africa/Cairo
/// conversion. This is what catches a date-format regression that a
/// parses-without-error test would miss entirely.
#[test]
fn parsed_dates_agree_with_the_whatsapp_envelope() {
    use chrono::{DateTime, Duration, Utc};

    let templates = seeded_templates();
    let fixtures: Vec<Fixture> = serde_json::from_str(BANK_SMS).unwrap();

    for f in &fixtures {
        // Pass the fixture's own arrival time: formats that omit the year
        // resolve against it, exactly as they do in production.
        let outcome = parse(
            &f.body,
            &templates,
            &no_noise(),
            triage::DEFAULT_THRESHOLD,
            f.wa_timestamp.parse::<chrono::DateTime<chrono::Utc>>().ok(),
        );
        let parsed = outcome.occurred_at.expect("bank SMS must have a date");
        let envelope: DateTime<Utc> = f.wa_timestamp.parse().unwrap();

        // The SMS is sent moments before WhatsApp receives it. Anything beyond a
        // few minutes means the date or the timezone is wrong.
        let skew = (parsed - envelope).num_seconds().abs();
        assert!(
            skew < Duration::minutes(10).num_seconds(),
            "{}: parsed {} vs envelope {} ({}s apart) -- template {:?}",
            f.wa_message_id,
            parsed,
            envelope,
            skew,
            outcome.template
        );
    }
}

/// A skeleton marked as noise must auto-ignore, even for something that would
/// otherwise score above the threshold. This is what stops the review queue
/// refilling with the same false positive after every reparse.
#[test]
fn noise_skeletons_short_circuit_triage() {
    let templates = seeded_templates();
    let body = "ALERT: EGP 1250.75 debited from card **9911 on 03/02/2026 at 09:15 ref FT26034ABCDE";

    let normal = parse(body, &templates, &no_noise(), triage::DEFAULT_THRESHOLD, None);
    assert_ne!(normal.status, ParseStatus::Ignored);

    let mut noise = HashSet::new();
    noise.insert(normal.skeleton_hash.clone());

    let suppressed = parse(body, &templates, &noise, triage::DEFAULT_THRESHOLD, None);
    assert_eq!(suppressed.status, ParseStatus::Ignored);
    assert!(suppressed.triage_signals.contains(&"known_noise_skeleton"));
}

/// Every message ends in exactly one terminal status and always keeps a skeleton
/// hash -- including ignored ones, because recurrence count is what separates
/// "a bank changed format" from "a human message".
#[test]
fn every_message_gets_a_terminal_status_and_a_skeleton() {
    let templates = seeded_templates();
    let mut all: Vec<Fixture> = serde_json::from_str(BANK_SMS).unwrap();
    all.extend(serde_json::from_str::<Vec<Fixture>>(CHATTER).unwrap());

    for f in &all {
        let o = parse(&f.body, &templates, &no_noise(), triage::DEFAULT_THRESHOLD, None);
        assert!(
            !o.skeleton_hash.is_empty(),
            "{}: no skeleton hash",
            f.wa_message_id
        );
        assert!(matches!(
            o.status,
            ParseStatus::Parsed | ParseStatus::Partial | ParseStatus::Unmatched | ParseStatus::Ignored
        ));
        assert!((0..=100).contains(&o.confidence));
    }
}

/// Threshold sweep over the real corpus. Not part of the normal run; this is the
/// tool that chose `triage::DEFAULT_THRESHOLD`.
///
///   cargo test threshold_sweep -- --ignored --nocapture
#[test]
#[ignore]
fn threshold_sweep() {
    let templates = seeded_templates();
    let bank: Vec<Fixture> = serde_json::from_str(BANK_SMS).unwrap();
    let chatter: Vec<Fixture> = serde_json::from_str(CHATTER).unwrap();

    println!("\nthreshold | bank SMS lost | chatter queued | notes");
    println!("----------|---------------|----------------|------");
    for threshold in [0, 10, 20, 30, 35, 40, 45, 50, 55, 60, 70, 80] {
        let lost = bank
            .iter()
            .filter(|f| {
                parse(&f.body, &templates, &no_noise(), threshold, None).status == ParseStatus::Ignored
            })
            .count();
        let queued = chatter
            .iter()
            .filter(|f| {
                parse(&f.body, &templates, &no_noise(), threshold, None).status != ParseStatus::Ignored
            })
            .count();
        let note = if threshold == triage::DEFAULT_THRESHOLD { "<- DEFAULT" } else { "" };
        println!("{threshold:>9} | {lost:>13} | {queued:>14} | {note}");
    }

    println!("\nscore distribution:");
    let mut bank_scores: Vec<i16> = bank
        .iter()
        .map(|f| parse(&f.body, &templates, &no_noise(), 0, None).triage_score)
        .collect();
    let mut chat_scores: Vec<i16> = chatter
        .iter()
        .map(|f| parse(&f.body, &templates, &no_noise(), 0, None).triage_score)
        .collect();
    bank_scores.sort();
    chat_scores.sort();
    println!(
        "  bank SMS  n={} min={} p50={} max={}",
        bank_scores.len(), bank_scores[0], bank_scores[bank_scores.len()/2], bank_scores[bank_scores.len()-1]
    );
    println!(
        "  chatter   n={} min={} p50={} p95={} max={}",
        chat_scores.len(), chat_scores[0], chat_scores[chat_scores.len()/2],
        chat_scores[chat_scores.len()*95/100], chat_scores[chat_scores.len()-1]
    );

    /// PetroApp transfers are recognised and then deliberately dropped.
    ///
    /// Fuel bought through PetroApp already reaches the system as a fuel_event
    /// via the PetroApp sync, so recording the bank's notification as well would
    /// count the same money twice and inflate every fuel and total figure.
    #[test]
    fn petroapp_messages_are_recognised_then_suppressed() {
        let templates = seeded_templates();
        let fixtures: Vec<Fixture> = serde_json::from_str(SUPPRESSED).unwrap();
        assert!(!fixtures.is_empty(), "suppressed fixture set is empty");

        for f in &fixtures {
            let normalized = normalize::normalize(&f.body);

            // Recognised: some template does match it. An unrecognised message is
            // a gap to chase; a suppressed one is a decision.
            assert!(
                templates.iter().any(|t| t.regex.is_match(&normalized)),
                "{}: no template matched, so suppression is hiding a parse gap",
                f.wa_message_id
            );

            let outcome = parse(
                &f.body,
                &templates,
                &no_noise(),
                triage::DEFAULT_THRESHOLD,
                f.wa_timestamp.parse::<chrono::DateTime<chrono::Utc>>().ok(),
            );
            assert_eq!(
                outcome.status,
                ParseStatus::Ignored,
                "{}: PetroApp must not reach the ledger",
                f.wa_message_id
            );
            assert!(
                outcome.triage_signals.contains(&"petroapp_suppressed"),
                "{}: ignored for the wrong reason -- signals {:?}",
                f.wa_message_id,
                outcome.triage_signals
            );
        }
    }
}
