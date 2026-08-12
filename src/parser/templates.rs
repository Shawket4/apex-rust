//! The template registry: DB-resident regexes with named capture groups.
//!
//! Patterns are stored in POST-normalization form (`الي` not `إلى`, `بطاقه`
//! not `بطاقة`) because matching runs on `normalize()` output. A pattern
//! written against raw text compiles fine, inserts fine, and can never match —
//! which is why every template carries a `sample`: a real message it must
//! match and fully extract. Writes validate against the sample; boot re-checks
//! every enabled template and disables (loudly) any that fail.
//!
//! No cache: the poller reloads the handful of rows each cycle, so a psql
//! INSERT is live within a minute and there is no invalidation path to get
//! wrong.

use chrono::{DateTime, Datelike, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Africa::Cairo;
use regex::Regex;
use rust_decimal::Decimal;
use sqlx::Row;
use std::collections::HashMap;
use std::str::FromStr;

use super::Extracted;
use crate::errors::{AppError, AppResult};

#[derive(Debug, Clone)]
pub struct CompiledTemplate {
    pub id: i64,
    pub name: String,
    pub regex: Regex,
    pub date_formats: Vec<String>,
    pub direction_map: HashMap<String, String>,
    pub sample: String,
    pub priority: i32,
}

/// Load every enabled template, priority order. A row whose regex no longer
/// compiles is skipped with an error log — never a silent drop, never fatal.
pub async fn load(pool: &sqlx::PgPool) -> AppResult<Vec<CompiledTemplate>> {
    let rows = sqlx::query(
        "SELECT id, name, pattern, date_formats, direction_map, sample, priority
         FROM banksms.parse_templates
         WHERE enabled
         ORDER BY priority, id",
    )
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let name: String = r.get("name");
        let pattern: String = r.get("pattern");
        match Regex::new(&pattern) {
            Ok(regex) => out.push(CompiledTemplate {
                id: r.get("id"),
                name,
                regex,
                date_formats: serde_json::from_value(r.get("date_formats")).unwrap_or_default(),
                direction_map: serde_json::from_value(r.get("direction_map")).unwrap_or_default(),
                sample: r.get("sample"),
                priority: r.get("priority"),
            }),
            Err(e) => {
                log::error!("template '{name}' regex no longer compiles, skipping: {e}");
            }
        }
    }
    Ok(out)
}

/// Extract fields from a successful regex match. Returns None unless
/// direction, amount AND occurred_at all resolve — the three fields a
/// transaction cannot exist without.
pub fn apply(
    t: &CompiledTemplate,
    caps: &regex::Captures<'_>,
    reference: Option<DateTime<Utc>>,
) -> Option<Extracted> {
    let group = |name: &str| -> Option<String> {
        caps.name(name)
            .map(|m| m.as_str().trim().to_string())
            .filter(|s| !s.is_empty())
    };

    // Direction: the action word through the template's own map, with the abk
    // fallback — "IPN transfer to X" is out, "from X" is in.
    let action = group("action").or_else(|| group("action_ar"));
    let mut direction = action.and_then(|a| t.direction_map.get(&a).cloned());
    if direction.is_none() {
        direction = match group("preposition").as_deref() {
            Some("to") => Some("out".into()),
            Some("from") => Some("in".into()),
            _ => None,
        };
    }
    let direction = direction?;

    let amount = parse_money(&group("amount")?)?;
    if amount <= Decimal::ZERO {
        return None;
    }

    let occurred_at = parse_datetime(
        &group("date")?,
        group("time").as_deref(),
        &t.date_formats,
        reference,
    )?;

    Some(Extracted {
        direction,
        amount,
        currency: group("currency")
            .map(|c| normalize_currency(&c))
            .unwrap_or_else(|| "EGP".to_string()),
        occurred_at,
        account: group("account"),
        counterparty: group("counterparty"),
        reference: group("reference"),
    })
}

/// Validate a template row the way the write path and the boot check both use:
/// the regex compiles, declares the required groups, matches its own
/// normalized sample, and extracts direction+amount+date from it end to end.
/// This is the assertion that makes "pattern can never match" and "direction
/// resolves to NULL" impossible to ship.
pub fn validate(
    pattern: &str,
    date_formats: &[String],
    direction_map: &HashMap<String, String>,
    sample: &str,
) -> AppResult<()> {
    let regex = Regex::new(pattern)
        .map_err(|e| AppError::BadRequest(format!("pattern does not compile: {e}")))?;

    for required in ["amount", "date"] {
        if !regex.capture_names().flatten().any(|n| n == required) {
            return Err(AppError::BadRequest(format!(
                "pattern must define a (?P<{required}>...) group"
            )));
        }
    }
    for (_, dir) in direction_map {
        if dir != "in" && dir != "out" {
            return Err(AppError::BadRequest(format!(
                "direction_map values must be 'in' or 'out', got '{dir}'"
            )));
        }
    }

    let normalized = super::normalize::normalize(sample);
    let caps = regex.captures(&normalized).ok_or_else(|| {
        AppError::BadRequest(
            "pattern does not match its own sample after normalization — \
             patterns must be written in post-normalization form"
                .to_string(),
        )
    })?;

    let probe = CompiledTemplate {
        id: 0,
        name: "candidate".into(),
        regex: regex.clone(),
        date_formats: date_formats.to_vec(),
        direction_map: direction_map.clone(),
        sample: sample.to_string(),
        priority: 0,
    };
    // `now` as reference keeps year-less samples valid whenever boot happens.
    apply(&probe, &caps, Some(Utc::now())).ok_or_else(|| {
        AppError::BadRequest(
            "pattern matches its sample but direction, amount or date failed to \
             extract — check direction_map covers the action word and \
             date_formats fits the sample's date"
                .to_string(),
        )
    })?;

    Ok(())
}

/// Boot check: every enabled template must pass `validate` against its own
/// sample. Failures are logged at ERROR and reported back so the caller can
/// disable the row and fire a notification — never a silent drain.
pub async fn boot_check(pool: &sqlx::PgPool) -> AppResult<Vec<String>> {
    let templates = load(pool).await?;
    let mut broken = Vec::new();
    for t in &templates {
        if let Err(e) = validate(
            t.regex.as_str(),
            &t.date_formats,
            &t.direction_map,
            &t.sample,
        ) {
            log::error!("template '{}' fails its own sample: {e}", t.name);
            sqlx::query(
                "UPDATE banksms.parse_templates SET enabled = FALSE, updated_at = now()
                 WHERE id = $1",
            )
            .bind(t.id)
            .execute(pool)
            .await?;
            broken.push(t.name.clone());
        }
    }
    Ok(broken)
}

/// Map whatever the bank wrote to an ISO-4217 code. `جم` and `ج.م` are
/// Egyptian pounds; storing them verbatim would split the same money across
/// two currency codes. Anything unrecognised passes through uppercased so a
/// genuinely new currency shows up as itself instead of silently becoming EGP.
pub fn normalize_currency(raw: &str) -> String {
    match raw.trim() {
        "جم" | "ج.م" | "جنيه" => "EGP".to_string(),
        other => other.to_uppercase(),
    }
}

/// Parse an amount. Thousands separators stripped; Decimal end to end, so
/// 2002.00 stays exactly 2002.00.
pub fn parse_money(s: &str) -> Option<Decimal> {
    Decimal::from_str(&s.replace(',', "").replace(' ', "")).ok()
}

/// Combine a date and optional time using the TEMPLATE'S OWN format list, then
/// convert Africa/Cairo → UTC.
///
/// The per-template format list is the whole point: `8/6/26` parses under both
/// `%m/%d/%y` and `%d/%m/%y` with different meanings, so a shared parser would
/// silently produce wrong dates rather than erroring. Cairo, not a fixed
/// offset — Egypt observes DST.
pub fn parse_datetime(
    date: &str,
    time: Option<&str>,
    formats: &[String],
    reference: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    let time = time.unwrap_or("00:00");
    let time_formats = ["%H:%M:%S", "%H:%M"];

    for df in formats {
        // No year in the format → the year comes from the message's own
        // arrival time, the only trustworthy source.
        if !df.contains("%Y") && !df.contains("%y") {
            if let Some(dt) = parse_yearless(date, time, df, reference) {
                return Some(dt);
            }
            continue;
        }

        for tf in time_formats {
            let combined = format!("{date} {time}");
            let fmt = format!("{df} {tf}");
            if let Ok(naive) = NaiveDateTime::parse_from_str(&combined, &fmt) {
                // Ambiguous/non-existent local times exist across DST
                // boundaries; take the earliest valid interpretation.
                return Cairo
                    .from_local_datetime(&naive)
                    .earliest()
                    .map(|dt| dt.with_timezone(&Utc));
            }
        }
    }
    None
}

/// Resolve a date that carries no year, using the message's arrival time.
///
/// Candidate years are [reference year, reference year − 1] in Cairo time: a
/// message arriving 2 January reporting "12-31" means LAST year. Anything
/// landing more than a week ahead of the reference is rejected — the slack
/// absorbs clock skew and late delivery without letting an old date read as
/// this year.
fn parse_yearless(
    date: &str,
    time: &str,
    date_format: &str,
    reference: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    // Without a reference there is nothing to infer from; inventing a year
    // would silently file the row under the wrong one.
    let reference = reference?;
    let ref_cairo = reference.with_timezone(&Cairo);
    let ref_year = ref_cairo.year();

    for tf in ["%H:%M:%S", "%H:%M"] {
        let fmt = format!("{date_format} {tf}");
        // Probe against a leap year so 29 February is never rejected outright.
        let probe = format!("2024 {date} {time}");
        let probe_fmt = format!("%Y {fmt}");
        let Ok(naive) = NaiveDateTime::parse_from_str(&probe, &probe_fmt) else {
            continue;
        };

        for candidate_year in [ref_year, ref_year - 1] {
            let Some(dated) = naive.with_year(candidate_year) else {
                continue; // 29 Feb in a non-leap year
            };
            let Some(utc) = Cairo
                .from_local_datetime(&dated)
                .earliest()
                .map(|dt| dt.with_timezone(&Utc))
            else {
                continue;
            };
            if utc <= reference + chrono::Duration::days(7) {
                return Some(utc);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Each template's sample date must resolve to the correct calendar day
    /// under its OWN format list. A shared date parser fails this.
    #[test]
    fn per_template_date_formats_resolve_correctly() {
        // (formats, date, time, expected Cairo Y-M-D)
        let cases: [(&[&str], &str, &str, (i32, u32, u32)); 5] = [
            (&["%m/%d/%y", "%m/%d/%Y"], "8/6/26", "19:08", (2026, 8, 6)), // abk: month-first
            (&["%d-%m-%Y"], "12-08-2026", "19:52", (2026, 8, 12)),        // arabic_ipn
            (&["%d/%m/%Y"], "09/08/2026", "08:16", (2026, 8, 9)),         // ref_balance
            (&["%d/%m/%y"], "08/08/26", "10:32", (2026, 8, 8)),           // cib_card
            (&["%d-%m-%Y"], "11-08-2026", "10:43", (2026, 8, 11)),        // instant_*
        ];
        for (formats, date, time, (y, m, d)) in cases {
            let formats: Vec<String> = formats.iter().map(|s| s.to_string()).collect();
            let got = parse_datetime(date, Some(time), &formats, None)
                .unwrap_or_else(|| panic!("failed to parse {date}"));
            let cairo = got.with_timezone(&Cairo);
            assert_eq!(
                (cairo.year(), cairo.month(), cairo.day()),
                (y, m, d),
                "wrong day for {date}"
            );
        }
    }

    /// A day>12 date must fail month-first parsing rather than silently
    /// swapping: 25/6/26 is only valid day-first.
    #[test]
    fn month_first_rejects_day_first_dates() {
        let formats = vec!["%m/%d/%y".to_string()];
        assert!(parse_datetime("25/6/26", Some("10:00"), &formats, None).is_none());
    }

    /// Cairo DST: +3 in August, +2 in January.
    #[test]
    fn cairo_offset_follows_dst() {
        let formats = vec!["%d-%m-%Y".to_string()];
        let aug = parse_datetime("12-08-2026", Some("12:00"), &formats, None).unwrap();
        assert_eq!(aug.format("%H:%M").to_string(), "09:00"); // UTC+3
        let jan = parse_datetime("12-01-2026", Some("12:00"), &formats, None).unwrap();
        assert_eq!(jan.format("%H:%M").to_string(), "10:00"); // UTC+2
    }

    /// Year-less %m-%d resolves against the arrival time; month-first verified
    /// against live traffic (arrived Aug 10 → "08-10").
    #[test]
    fn yearless_month_first_resolves_from_reference() {
        let reference = Utc.with_ymd_and_hms(2026, 8, 10, 7, 50, 50).unwrap();
        let formats = vec!["%m-%d".to_string()];
        let got = parse_datetime("08-10", Some("10:50"), &formats, Some(reference)).unwrap();
        let cairo = got.with_timezone(&Cairo);
        assert_eq!((cairo.year(), cairo.month(), cairo.day()), (2026, 8, 10));
    }

    /// The year boundary: arriving 2 Jan reporting 12-31 means LAST year.
    #[test]
    fn yearless_year_boundary_resolves_to_previous_year() {
        let reference = Utc.with_ymd_and_hms(2027, 1, 2, 9, 0, 0).unwrap();
        let formats = vec!["%m-%d".to_string()];
        let got = parse_datetime("12-31", Some("23:10"), &formats, Some(reference)).unwrap();
        let cairo = got.with_timezone(&Cairo);
        assert_eq!((cairo.year(), cairo.month(), cairo.day()), (2026, 12, 31));
    }

    /// No reference → no date, never a guessed year.
    #[test]
    fn yearless_without_reference_refuses() {
        let formats = vec!["%m-%d".to_string()];
        assert!(parse_datetime("08-10", Some("10:50"), &formats, None).is_none());
    }

    #[test]
    fn currency_folds_arabic_pound_spellings() {
        assert_eq!(normalize_currency("جم"), "EGP");
        assert_eq!(normalize_currency("ج.م"), "EGP");
        assert_eq!(normalize_currency("EGP"), "EGP");
        assert_eq!(normalize_currency("usd"), "USD");
    }

    #[test]
    fn money_keeps_exact_decimals() {
        assert_eq!(parse_money("2,002.00").unwrap().to_string(), "2002.00");
        assert_eq!(parse_money("30.5").unwrap().to_string(), "30.5");
        assert!(parse_money("garbage").is_none());
    }
}
