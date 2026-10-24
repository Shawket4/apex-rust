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

use chrono::{DateTime, Datelike, NaiveDateTime, TimeZone, Timelike, Utc};
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

    // A few messages carry no date at all ("Your debit card **1234 was
    // debited by ..."). Their template opts in with the `@received` marker and
    // the transaction takes the message's arrival time, which for these
    // real-time notifications is within a minute of the payment.
    let occurred_at = if uses_arrival_time(&t.date_formats) {
        reference?
    } else {
        let dt = parse_datetime(
            &group("date")?,
            group("time").as_deref(),
            &t.date_formats,
            reference,
        )?;
        if has_marker(&t.date_formats, TWELVE_HOUR_CLOCK) {
            resolve_twelve_hour(dt, reference)
        } else {
            dt
        }
    };

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

    let required: &[&str] = if uses_arrival_time(date_formats) {
        &["amount"]
    } else {
        &["amount", "date"]
    };
    for &required in required {
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
    // A date with no time ("في 20-AUG-2026"): when the message arrived that
    // same Cairo day, its arrival time is the best time there is; otherwise
    // midnight, as before.
    let arrival = match time {
        None => arrival_time_if_same_day(date, formats, reference),
        Some(_) => None,
    };
    let time = time.or(arrival.as_deref()).unwrap_or("00:00");
    let time_formats = ["%H:%M:%S", "%H:%M"];

    for df in formats {
        if is_marker(df) {
            continue;
        }
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

/// Markers a template's `date_formats` can carry beside its chrono formats.
/// They start with `@`, which no chrono format does.
///
/// `@received`: the messages carry no date, so the transaction is dated by
/// the message's arrival.
pub const ARRIVAL_TIME: &str = "@received";
/// `@12h`: the bank writes a 12-hour clock with no AM/PM ("at 08:15" for
/// 20:15); the arrival time decides which reading it is.
pub const TWELVE_HOUR_CLOCK: &str = "@12h";

fn is_marker(format: &str) -> bool {
    format.starts_with('@')
}

fn has_marker(formats: &[String], marker: &str) -> bool {
    formats.iter().any(|f| f == marker)
}

pub fn uses_arrival_time(formats: &[String]) -> bool {
    has_marker(formats, ARRIVAL_TIME)
}

/// A 12-hour time without AM/PM has two readings, twelve hours apart. Keep
/// the latest one that is not after the message arrived (with a few minutes
/// for clock skew): notices arrive minutes after the payment, never half a
/// day before it. With no arrival time to go by, the text stands as written.
fn resolve_twelve_hour(dt: DateTime<Utc>, reference: Option<DateTime<Utc>>) -> DateTime<Utc> {
    let Some(arrived) = reference else {
        return dt;
    };
    let local = dt.with_timezone(&Cairo).naive_local();
    let half_day = chrono::Duration::hours(12);
    let readings = match local.hour() {
        0..=11 => vec![local, local + half_day],
        12 => vec![local - half_day, local],
        _ => vec![local], // 13:00-23:59 can only be one time
    };
    let readings: Vec<DateTime<Utc>> = readings
        .iter()
        .filter_map(|n| Cairo.from_local_datetime(n).earliest())
        .map(|d| d.with_timezone(&Utc))
        .collect();
    let latest_allowed = arrived + chrono::Duration::minutes(15);
    readings
        .iter()
        .copied()
        .filter(|d| *d <= latest_allowed)
        .max()
        .or_else(|| readings.iter().copied().min())
        .unwrap_or(dt)
}

/// The arrival time (Cairo, HH:MM:SS) when the message arrived on the same
/// Cairo day as its date-only text says; None when it didn't, or when the
/// date doesn't parse under any full-date format of the template.
fn arrival_time_if_same_day(
    date: &str,
    formats: &[String],
    reference: Option<DateTime<Utc>>,
) -> Option<String> {
    let arrived = reference?.with_timezone(&Cairo);
    for df in formats {
        if is_marker(df) || (!df.contains("%Y") && !df.contains("%y")) {
            continue;
        }
        if let Ok(day) = chrono::NaiveDate::parse_from_str(date, df) {
            return (day == arrived.date_naive()).then(|| arrived.format("%H:%M:%S").to_string());
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

    /// "18-AUG-2026" with no time: same-day arrival lends its time, a later
    /// arrival falls back to midnight, and the month name is case-blind.
    #[test]
    fn date_only_takes_the_arrival_time_on_the_same_day() {
        let formats = vec!["%d-%b-%Y".to_string()];
        let arrived = Utc.with_ymd_and_hms(2026, 8, 18, 9, 41, 20).unwrap(); // 12:41:20 Cairo
        let got = parse_datetime("18-AUG-2026", None, &formats, Some(arrived)).unwrap();
        assert_eq!(got, arrived);

        let next_day = Utc.with_ymd_and_hms(2026, 8, 19, 7, 0, 0).unwrap();
        let got = parse_datetime("18-AUG-2026", None, &formats, Some(next_day)).unwrap();
        assert_eq!(
            got.with_timezone(&Cairo)
                .format("%Y-%m-%d %H:%M")
                .to_string(),
            "2026-08-18 00:00"
        );
    }

    #[test]
    fn arrival_time_format_is_recognised() {
        assert!(uses_arrival_time(&["@received".to_string()]));
        assert!(!uses_arrival_time(&["%d/%m/%Y".to_string()]));
    }

    /// "at 03:10" on a 12-hour clock: the arrival at 15:16 makes it 15:10; an
    /// arrival at 07:42 keeps "07:40" in the morning; "12:05" arriving 12:11
    /// is just after noon; 13:00 and later are left alone.
    #[test]
    fn twelve_hour_clock_takes_the_reading_before_arrival() {
        let formats = vec!["%d/%m/%y".to_string(), "@12h".to_string()];
        let cairo = |s: &str| {
            Cairo
                .from_local_datetime(&NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap())
                .unwrap()
                .with_timezone(&Utc)
        };
        let fmt = |d: DateTime<Utc>| d.with_timezone(&Cairo).format("%Y-%m-%d %H:%M").to_string();
        for (date, time, arrived, want) in [
            ("11/09/26", "03:10", "2026-09-11 15:16", "2026-09-11 15:10"),
            ("02/10/26", "07:40", "2026-10-02 07:42", "2026-10-02 07:40"),
            ("27/09/26", "12:05", "2026-09-27 12:11", "2026-09-27 12:05"),
            ("19/10/26", "08:30", "2026-10-19 20:36", "2026-10-19 20:30"),
            ("19/10/26", "13:05", "2026-10-19 13:06", "2026-10-19 13:05"),
        ] {
            let parsed = parse_datetime(date, Some(time), &formats, None).unwrap();
            assert_eq!(
                fmt(resolve_twelve_hour(parsed, Some(cairo(arrived)))),
                want,
                "{date} {time}"
            );
        }
        // No arrival: the text as written.
        let parsed = parse_datetime("11/09/26", Some("03:10"), &formats, None).unwrap();
        assert_eq!(fmt(resolve_twelve_hour(parsed, None)), "2026-09-11 03:10");
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
