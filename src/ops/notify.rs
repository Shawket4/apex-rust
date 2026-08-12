//! ntfy push notifications for newly parsed bank messages.
//!
//! Every notification carries a deep link straight to the row's edit screen, so
//! the person reading it on a phone can add a note or fix a field without
//! hunting for it:
//!
//! ```text
//! https://apextransport.ddns.net/fleet-expenses/224/edit
//! ```
//!
//! Disabled unless `NTFY_TOPIC` is set, so a deploy without the config is silent
//! rather than noisy or broken.
//!
//! Delivery is fire-and-forget on a detached task. A notification that fails to
//! send must never fail a parse run or hold up ingestion -- the money is already
//! recorded; the push is a convenience.

use log::{debug, warn};
use rust_decimal::Decimal;

use crate::config::CONFIG;

/// What the parser found, reduced to what a notification needs.
#[derive(Debug, Clone)]
pub struct NewTransaction {
    pub id: i64,
    pub amount: Option<Decimal>,
    pub currency: Option<String>,
    pub direction: Option<String>,
    pub counterparty: Option<String>,
    pub account: Option<String>,
    pub reference: Option<String>,
    pub template: Option<String>,
}

impl NewTransaction {
    fn edit_url(&self) -> String {
        format!(
            "{}/fleet-expenses/{}/edit",
            CONFIG.dashboard_base_url.trim_end_matches('/'),
            self.id
        )
    }

    /// Body text. UTF-8 is fine here, unlike the headers, so Arabic
    /// counterparties are shown in full.
    fn body(&self) -> String {
        let mut lines = Vec::new();

        let amount = match (&self.amount, &self.currency) {
            (Some(a), Some(c)) => format!("{c} {a}"),
            (Some(a), None) => a.to_string(),
            _ => "unknown amount".to_string(),
        };
        let arrow = match self.direction.as_deref() {
            Some("in") => "in",
            _ => "out",
        };
        lines.push(format!("{amount} ({arrow})"));

        if let Some(cp) = self.counterparty.as_deref().filter(|s| !s.is_empty()) {
            lines.push(cp.to_string());
        }
        if let Some(acct) = self.account.as_deref().filter(|s| !s.is_empty()) {
            lines.push(format!("Account {acct}"));
        }
        if let Some(r) = self.reference.as_deref().filter(|s| !s.is_empty()) {
            lines.push(format!("Ref {r}"));
        }
        lines.push("Tap to add a note or correct it.".to_string());

        lines.join("\n")
    }
}

/// Strip a string to characters that survive an HTTP header.
///
/// ntfy reads titles and tags from headers, which are byte strings — a raw
/// Arabic counterparty in `X-Title` produces an invalid header and the whole
/// request is rejected. Non-ASCII is dropped here and the full text is carried
/// in the UTF-8 body instead.
fn header_safe(s: &str, max: usize) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii() && !c.is_ascii_control())
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }

    // The suffix is three ASCII dots, so three characters have to be reserved
    // for it or the result overruns `max`. A literal "…" would also reintroduce
    // the non-ASCII byte this function exists to remove.
    const SUFFIX: &str = "...";
    if max <= SUFFIX.len() {
        return trimmed.chars().take(max).collect();
    }
    let head: String = trimmed.chars().take(max - SUFFIX.len()).collect();
    format!("{}{SUFFIX}", head.trim_end())
}

fn enabled() -> bool {
    !CONFIG.ntfy_topic.is_empty()
}

async fn publish(title: String, body: String, tags: &str, priority: &str, click: Option<String>) {
    let url = format!(
        "{}/{}",
        CONFIG.ntfy_url.trim_end_matches('/'),
        CONFIG.ntfy_topic
    );

    let client = reqwest::Client::new();
    let mut req = client
        .post(&url)
        .header("X-Title", title)
        .header("X-Tags", tags)
        .header("X-Priority", priority)
        .body(body);

    if let Some(click) = &click {
        // X-Click makes tapping the notification open the row directly.
        // X-Actions adds an explicit button for clients that show one.
        req = req
            .header("X-Click", click.clone())
            .header("X-Actions", format!("view, Open, {click}, clear=true"));
    }

    if let Some(token) = &CONFIG.ntfy_token {
        req = req.bearer_auth(token);
    }

    match req.send().await {
        Ok(r) if r.status().is_success() => debug!("ntfy sent"),
        Ok(r) => warn!("ntfy rejected the message: HTTP {}", r.status()),
        Err(e) => warn!("ntfy send failed: {e}"),
    }
}

/// Announce transactions created by one parse run.
///
/// Batching rule: a handful get individual notifications with their own deep
/// links, because those are individually actionable. A large run — a backfill,
/// or a catch-up after downtime — collapses into one summary instead, since
/// fifty separate pushes are just noise nobody reads.
pub fn notify_new_transactions(items: Vec<NewTransaction>) {
    if !enabled() || items.is_empty() {
        return;
    }

    tokio::spawn(async move {
        if items.len() > CONFIG.ntfy_max_individual {
            let total: Decimal = items.iter().filter_map(|i| i.amount).sum();
            let url = format!(
                "{}/fleet-expenses",
                CONFIG.dashboard_base_url.trim_end_matches('/')
            );
            publish(
                format!("{} new bank transactions", items.len()),
                format!("Total EGP {total}\nParsed from WhatsApp. Tap to review them.",),
                "bank,receipt",
                "default",
                Some(url),
            )
            .await;
            return;
        }

        for item in items {
            let title = match (&item.amount, &item.currency) {
                (Some(a), Some(c)) => header_safe(&format!("{c} {a}"), 60),
                (Some(a), None) => header_safe(&a.to_string(), 60),
                _ => "New bank transaction".to_string(),
            };
            let click = item.edit_url();
            publish(title, item.body(), "bank,receipt", "default", Some(click)).await;
        }
    });
}

/// Operator alert: something needs a human (cutover pending, a template
/// failed its own sample at boot). High priority, no deep link.
pub async fn notify_ops(title: &str, body: &str) {
    if !enabled() {
        return;
    }
    publish(
        header_safe(title, 60),
        body.to_string(),
        "warning",
        "high",
        None,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn sample() -> NewTransaction {
        NewTransaction {
            id: 224,
            amount: Some(dec!(1500.00)),
            currency: Some("EGP".into()),
            direction: Some("out".into()),
            counterparty: Some("محمد ع** ع** ط*****".into()),
            account: Some("6001-01".into()),
            reference: Some("ac8fc63f".into()),
            template: Some("ref_balance".into()),
        }
    }

    #[test]
    fn edit_url_is_the_deep_link_the_operator_expects() {
        // Matches the shape requested: /fleet-expenses/<id>/edit
        assert!(sample().edit_url().ends_with("/fleet-expenses/224/edit"));
    }

    #[test]
    fn arabic_survives_in_the_body() {
        let body = sample().body();
        assert!(body.contains("محمد"));
        assert!(body.contains("EGP 1500.00"));
        assert!(body.contains("6001-01"));
        assert!(body.contains("ac8fc63f"));
    }

    /// The bug this guards: a non-ASCII header makes reqwest reject the request
    /// outright, so every notification for an Arabic counterparty would vanish.
    #[test]
    fn headers_are_stripped_to_ascii() {
        let out = header_safe("محمد ع** ع** ط***** EGP 1500", 60);
        assert!(out.is_ascii(), "header must be ASCII, got {out:?}");
        assert!(out.contains("EGP 1500"));
    }

    #[test]
    fn header_safe_truncates_without_panicking_on_multibyte() {
        let out = header_safe("تم خصم مبلغ EGP 15000.00 من حساب ********9276", 10);
        assert!(out.is_ascii());
        assert!(out.chars().count() <= 10);
    }

    #[test]
    fn header_safe_drops_control_characters() {
        assert_eq!(header_safe("EGP\n1500\r\n", 60), "EGP1500");
    }

    #[test]
    fn body_reports_direction() {
        let mut t = sample();
        t.direction = Some("in".into());
        assert!(t.body().contains("(in)"));
        t.direction = Some("out".into());
        assert!(t.body().contains("(out)"));
    }

    #[test]
    fn missing_amount_does_not_produce_a_broken_body() {
        let mut t = sample();
        t.amount = None;
        t.currency = None;
        assert!(t.body().contains("unknown amount"));
    }
}
