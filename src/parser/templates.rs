//! Tier 1: the DB-backed template registry.
//!
//! Patterns live in `banksms.parse_templates`, not in code, so a bank changing
//! its SMS format is a row insert plus a reparse rather than a redeploy. They are
//! compiled once into a cached set and the cache is invalidated when the registry
//! changes.
//!
//! Patterns are stored in POST-NORMALIZATION form -- see parser::normalize.

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Africa::Cairo;
use regex::Regex;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::RwLock;

use crate::errors::{AppError, AppResult};
use crate::parser::Direction;

/// Bump when parsing behaviour changes in a way that should trigger a reparse of
/// previously-unparsed rows. Rows already `parsed` by a NEWER version are never
/// touched by an older one.
pub const PARSER_VERSION: i32 = 1;

#[derive(Debug, Clone)]
pub struct CompiledTemplate {
    pub id: i64,
    pub name: String,
    pub regex: Regex,
    pub date_formats: Vec<String>,
    pub direction_map: HashMap<String, Direction>,
    pub priority: i32,
}

/// A tier-1 match: every field the template managed to capture.
#[derive(Debug, Clone, Default)]
pub struct TemplateMatch {
    pub template_name: String,
    pub direction: Option<Direction>,
    pub amount: Option<Decimal>,
    pub currency: Option<String>,
    pub account: Option<String>,
    pub counterparty: Option<String>,
    pub reference: Option<String>,
    pub balance_after: Option<Decimal>,
    pub occurred_at: Option<DateTime<Utc>>,
}

static CACHE: RwLock<Option<Vec<CompiledTemplate>>> = RwLock::new(None);

/// Drop the compiled cache. Called after any write to the registry.
pub fn invalidate_cache() {
    if let Ok(mut c) = CACHE.write() {
        *c = None;
    }
}

/// Load and compile every enabled template, using the cache when warm.
pub async fn load(pool: &sqlx::PgPool) -> AppResult<Vec<CompiledTemplate>> {
    if let Ok(cache) = CACHE.read() {
        if let Some(templates) = cache.as_ref() {
            return Ok(templates.clone());
        }
    }

    let rows = sqlx::query!(
        r#"
        SELECT id, name, pattern, date_formats, direction_map, priority
        FROM banksms.parse_templates
        WHERE enabled
        ORDER BY priority, id
        "#
    )
    .fetch_all(pool)
    .await?;

    let mut compiled = Vec::with_capacity(rows.len());
    for row in rows {
        // A pattern that fails to compile is skipped rather than fatal: one bad
        // hand-added row must not take the whole parser offline. The write path
        // validates on insert, so this should be unreachable.
        let regex = match Regex::new(&row.pattern) {
            Ok(r) => r,
            Err(e) => {
                log::error!(
                    "template '{}' (id {}) has an invalid pattern and was skipped: {e}",
                    row.name,
                    row.id
                );
                continue;
            }
        };

        let date_formats: Vec<String> =
            serde_json::from_value(row.date_formats).unwrap_or_default();
        let raw_map: HashMap<String, String> =
            serde_json::from_value(row.direction_map).unwrap_or_default();
        let direction_map = raw_map
            .into_iter()
            .filter_map(|(k, v)| Direction::from_str(&v).ok().map(|d| (k, d)))
            .collect();

        compiled.push(CompiledTemplate {
            id: row.id,
            name: row.name,
            regex,
            date_formats,
            direction_map,
            priority: row.priority,
        });
    }

    if let Ok(mut cache) = CACHE.write() {
        *cache = Some(compiled.clone());
    }

    Ok(compiled)
}

/// Validate a pattern before it enters the registry.
///
/// This has to happen in Rust, not as a CHECK constraint: Postgres POSIX regex
/// has no `(?P<name>...)` named capture groups, so a database-side test match
/// would reject every valid pattern here.
pub fn validate_pattern(pattern: &str) -> AppResult<()> {
    let re = Regex::new(pattern)
        .map_err(|e| AppError::BadRequest(format!("pattern does not compile: {e}")))?;

    let names: Vec<&str> = re.capture_names().flatten().collect();
    for required in ["amount"] {
        if !names.contains(&required) {
            return Err(AppError::BadRequest(format!(
                "pattern must define a named capture group '{required}'"
            )));
        }
    }
    if !names.contains(&"date") {
        return Err(AppError::BadRequest(
            "pattern must define a named capture group 'date'".to_string(),
        ));
    }
    Ok(())
}

/// Try each template in priority order; first match wins.
pub fn match_first(templates: &[CompiledTemplate], normalized: &str) -> Option<TemplateMatch> {
    templates
        .iter()
        .find_map(|t| apply(t, normalized).map(|m| m))
}

fn apply(t: &CompiledTemplate, normalized: &str) -> Option<TemplateMatch> {
    let caps = t.regex.captures(normalized)?;
    let get = |n: &str| caps.name(n).map(|m| m.as_str().trim().to_string());

    let direction = get("action")
        .or_else(|| get("action_ar"))
        .and_then(|a| t.direction_map.get(&a).copied())
        // `abk` encodes direction twice: the action word and a from/to
        // preposition. Fall back to the preposition when the action is absent.
        .or_else(|| match get("preposition").as_deref() {
            Some("to") => Some(Direction::Out),
            Some("from") => Some(Direction::In),
            _ => None,
        });

    let occurred_at = match (get("date"), get("time")) {
        (Some(d), time) => parse_datetime(&d, time.as_deref(), &t.date_formats),
        _ => None,
    };

    Some(TemplateMatch {
        template_name: t.name.clone(),
        direction,
        amount: get("amount").and_then(|s| parse_money(&s)),
        currency: get("currency"),
        account: get("account"),
        counterparty: get("counterparty").filter(|s| !s.is_empty()),
        reference: get("reference"),
        balance_after: get("balance").and_then(|s| parse_money(&s)),
        occurred_at,
    })
}

/// Parse an amount. Thousands separators are stripped; the value is Decimal, so
/// 2002.00 stays exactly 2002.00 rather than becoming 2001.9999999999998.
pub fn parse_money(s: &str) -> Option<Decimal> {
    Decimal::from_str(&s.replace(',', "").replace(' ', "")).ok()
}

/// Combine a date and optional time using the TEMPLATE'S OWN format list, then
/// convert Africa/Cairo -> UTC.
///
/// The per-template format list is the whole point. `8/6/26` parses successfully
/// as both `%m/%d/%y` (6 August) and `%d/%m/%y` (8 June); a shared date parser
/// would silently pick one and be wrong for three of the four templates rather
/// than erroring.
///
/// Cairo, not a fixed offset: Egypt observes DST, so the offset is +2 in winter
/// and +3 in summer.
pub fn parse_datetime(
    date: &str,
    time: Option<&str>,
    formats: &[String],
) -> Option<DateTime<Utc>> {
    let time = time.unwrap_or("00:00");
    // Bank SMS use both H:MM and HH:MM.
    let time_formats = ["%H:%M:%S", "%H:%M"];

    for df in formats {
        for tf in time_formats {
            let combined = format!("{date} {time}");
            let fmt = format!("{df} {tf}");
            if let Ok(naive) = NaiveDateTime::parse_from_str(&combined, &fmt) {
                // A local time can be ambiguous or non-existent across a DST
                // boundary; take the earliest valid interpretation rather than
                // dropping the row.
                return Cairo
                    .from_local_datetime(&naive)
                    .earliest()
                    .map(|dt| dt.with_timezone(&Utc));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    /// The test the spec demands: all four templates' sample dates must resolve
    /// to 6 August 2026, each under its OWN format. A shared parser fails this.
    #[test]
    fn each_template_date_format_resolves_to_the_same_day() {
        let cases = [
            ("abk", "8/6/26", vec!["%m/%d/%y".to_string()]),
            ("arabic_ipn", "06-08-2026", vec!["%d-%m-%Y".to_string()]),
            ("ref_balance", "06/08/2026", vec!["%d/%m/%Y".to_string()]),
            ("cib_card", "06/08/26", vec!["%d/%m/%y".to_string()]),
        ];

        for (name, date, formats) in cases {
            let dt = parse_datetime(date, Some("12:00"), &formats)
                .unwrap_or_else(|| panic!("{name}: {date} failed to parse"));
            let cairo = dt.with_timezone(&Cairo);
            assert_eq!(
                cairo.format("%Y-%m-%d").to_string(),
                "2026-08-06",
                "{name}: {date} resolved to the wrong day"
            );
        }
    }

    /// The specific silent-corruption case: the same string, two formats, two
    /// different real dates.
    #[test]
    fn ambiguous_date_means_different_days_under_different_formats() {
        let month_first = parse_datetime("8/6/26", Some("12:00"), &["%m/%d/%y".into()]).unwrap();
        let day_first = parse_datetime("8/6/26", Some("12:00"), &["%d/%m/%y".into()]).unwrap();
        assert_ne!(month_first, day_first);
        assert_eq!(
            month_first.with_timezone(&Cairo).format("%Y-%m-%d").to_string(),
            "2026-08-06"
        );
        assert_eq!(
            day_first.with_timezone(&Cairo).format("%Y-%m-%d").to_string(),
            "2026-06-08"
        );
    }

    #[test]
    fn cairo_is_utc_plus_3_in_summer_and_plus_2_in_winter() {
        // Egypt observes DST; a fixed offset would be wrong half the year.
        let summer = parse_datetime("08/08/2026", Some("14:32"), &["%d/%m/%Y".into()]).unwrap();
        assert_eq!(summer.format("%H:%M").to_string(), "11:32");

        let winter = parse_datetime("01/11/2025", Some("14:32"), &["%d/%m/%Y".into()]).unwrap();
        assert_eq!(winter.format("%H:%M").to_string(), "12:32");
    }

    #[test]
    fn money_keeps_exact_decimal_values() {
        assert_eq!(parse_money("2002.00").unwrap(), dec!(2002.00));
        assert_eq!(parse_money("20,200.00").unwrap(), dec!(20200.00));
        assert_eq!(parse_money("500.5").unwrap(), dec!(500.5));
        assert_eq!(parse_money("1001.0").unwrap(), dec!(1001.0));
    }

    #[test]
    fn single_digit_hour_times_parse() {
        // "8/6/26 9:05" occurs in ABK messages.
        assert!(parse_datetime("8/6/26", Some("9:05"), &["%m/%d/%y".into()]).is_some());
    }

    #[test]
    fn validate_pattern_rejects_uncompilable() {
        assert!(validate_pattern(r"(?P<amount>[\d]+) (?P<date>\d+").is_err());
    }

    #[test]
    fn validate_pattern_requires_amount_and_date_groups() {
        assert!(validate_pattern(r"(?P<amount>[\d]+)").is_err());
        assert!(validate_pattern(r"(?P<date>[\d/]+)").is_err());
        assert!(validate_pattern(r"(?P<amount>[\d]+) on (?P<date>[\d/]+)").is_ok());
    }
}
