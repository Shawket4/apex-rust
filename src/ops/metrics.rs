//! Process counters, exposed in Prometheus text format.
//!
//! The categories are deliberately NOT lumped together. `ignored` is chat noise:
//! expected, high-volume (~93% of messages in the target chat), and must never
//! page anyone. `partial` and `unmatched` mean something looked like a bank SMS
//! and did not parse -- that is the number worth watching.

use std::sync::atomic::{AtomicU64, Ordering};

macro_rules! counters {
    ($($name:ident),* $(,)?) => {
        $(pub static $name: AtomicU64 = AtomicU64::new(0);)*
    };
}

counters! {
    MESSAGES_INGESTED,
    MESSAGES_PARSED,
    MESSAGES_PARTIAL,
    MESSAGES_UNMATCHED,
    MESSAGES_IGNORED,
    TRANSACTIONS_CREATED,
    POLL_CYCLES,
    POLL_ERRORS,
    POLL_SKIPPED_LOCKED,
    PARSE_ERRORS,
    AUTH_FAILURES,
    ALARM_UNMATCHED_SKELETONS,
}

pub fn incr(counter: &AtomicU64, by: u64) {
    counter.fetch_add(by, Ordering::Relaxed);
}

pub fn get(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

/// Render counters in Prometheus text exposition format.
pub fn render() -> String {
    let metrics: [(&str, &str, u64); 12] = [
        ("banksms_messages_ingested_total", "Raw messages stored", get(&MESSAGES_INGESTED)),
        ("banksms_messages_parsed_total", "Messages parsed via a template", get(&MESSAGES_PARSED)),
        ("banksms_messages_partial_total", "Looked like a bank SMS, parsed incompletely", get(&MESSAGES_PARTIAL)),
        ("banksms_messages_unmatched_total", "Looked like a bank SMS, did not parse", get(&MESSAGES_UNMATCHED)),
        ("banksms_messages_ignored_total", "Chat noise rejected by triage - expected, not an alarm", get(&MESSAGES_IGNORED)),
        ("banksms_transactions_created_total", "Transactions materialised from messages", get(&TRANSACTIONS_CREATED)),
        ("banksms_poll_cycles_total", "Completed poll cycles", get(&POLL_CYCLES)),
        ("banksms_poll_errors_total", "Failed poll cycles", get(&POLL_ERRORS)),
        ("banksms_poll_skipped_locked_total", "Cycles skipped because another instance held the lock", get(&POLL_SKIPPED_LOCKED)),
        ("banksms_parse_errors_total", "Parse runs that failed", get(&PARSE_ERRORS)),
        ("banksms_auth_failures_total", "Rejected tokens", get(&AUTH_FAILURES)),
        ("banksms_unmatched_skeletons", "Recurring formats with no template - THE alarm condition", get(&ALARM_UNMATCHED_SKELETONS)),
    ];

    let mut out = String::with_capacity(2048);
    for (name, help, value) in metrics {
        // The alarm is a point-in-time count of currently-unhandled formats, so
        // it is a gauge -- it goes down when a template is added. Labelling it a
        // counter would make rate() queries over it meaningless.
        let kind = if name == "banksms_unmatched_skeletons" { "gauge" } else { "counter" };
        out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_emits_prometheus_format() {
        incr(&MESSAGES_INGESTED, 7);
        let out = render();
        assert!(out.contains("# TYPE banksms_messages_ingested_total counter"));
        assert!(out.contains("banksms_messages_ingested_total 7"));
    }

    /// The categories must stay separate: conflating them is how an expected,
    /// high-volume signal ends up paging someone at 3am.
    #[test]
    fn ignored_and_unmatched_are_distinct_metrics() {
        let out = render();
        assert!(out.contains("banksms_messages_ignored_total"));
        assert!(out.contains("banksms_messages_unmatched_total"));
    }
}
