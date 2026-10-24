//! The scheduled digest: the advisor's own look at the fleet, written for the
//! owner, every morning (yesterday) and every Saturday (the week before).
//!
//! A run started from the UI uses the caller's token. The scheduled run has no
//! caller, so it mints a short-lived read token for `ADVISOR_DIGEST_USER_ID`
//! (an admin, normally the owner) — enough for etit-proxy and maint-rust to
//! answer. It cannot change anything: whatever it proposes waits as a
//! suggestion until a human approves it with their own token.

use std::sync::Mutex;
use std::time::Duration;

use chrono::{Datelike, Duration as Days, NaiveDate, Timelike, Utc, Weekday};
use chrono_tz::Africa::Cairo;
use sqlx::PgPool;

use super::tools::ToolCtx;
use super::upstream::BearerToken;
use super::{agent, prompt, store, Advisor};

/// Write and store the digest for the `days` ending yesterday (Cairo).
pub async fn write(
    pool: &PgPool,
    adv: &Advisor,
    token: &BearerToken,
    user_id: i64,
    days: i64,
) -> Result<uuid::Uuid, String> {
    let provider = adv.provider.clone().ok_or("advisor not configured")?;
    let today = Utc::now().with_timezone(&Cairo).date_naive();
    let end = today - Days::days(1);
    let start = end - Days::days(days - 1);
    let ctx = ToolCtx {
        pool,
        adv,
        token,
        user_id,
        conversation_id: None,
        proposed: Mutex::new(Vec::new()),
        events: None,
    };
    let task = format!(
        "{}\n\nPeriod: {start} to {end} ({days} day{}). Previous period for comparison: {} to {}.",
        prompt::DIGEST_TASK,
        if days == 1 { "" } else { "s" },
        start - Days::days(days),
        start - Days::days(1),
    );
    let out = agent::run(provider.as_ref(), &prompt::system(), Vec::new(), &task, &ctx)
        .await
        .map_err(|e| e.to_string())?;
    store::save_digest(pool, start, end, &out.answer, &provider.name())
        .await
        .map_err(|e| e.to_string())
}

/// A read token for the scheduled run, valid ten minutes. Claims mirror
/// FalconGo's (maint-rust requires `iss`).
fn mint_token(user_id: i64) -> Option<BearerToken> {
    let secret = &crate::config::CONFIG.jwt_secret;
    let exp = (Utc::now() + Days::minutes(10)).timestamp();
    let claims = serde_json::json!({
        "user_type": "admin_user",
        "user_id": user_id,
        "driver_id": 0,
        "permission": 4,
        "iss": user_id.to_string(),
        "exp": exp,
    });
    jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
    )
    .ok()
    .map(BearerToken)
}

/// Which digests are due on `day` at 07:00: always yesterday; on Saturday,
/// also the seven days before (Egypt's working week starts Saturday).
fn due_windows(day: NaiveDate) -> Vec<i64> {
    let mut w = vec![1];
    if day.weekday() == Weekday::Sat {
        w.push(7);
    }
    w
}

/// The background loop, started from main. Wakes every ten minutes and, from
/// 07:00 Cairo, writes whatever digest for today is not stored yet — so a
/// restart at 07:05 does not skip the day, and two runs never duplicate.
pub async fn run_schedule(pool: PgPool, adv: Advisor) {
    let user_id = match std::env::var("ADVISOR_DIGEST_USER_ID").ok().and_then(|v| v.trim().parse::<i64>().ok()) {
        Some(id) if id > 0 => id,
        _ => {
            log::info!("advisor digest schedule off (ADVISOR_DIGEST_USER_ID not set)");
            return;
        }
    };
    if adv.provider.is_none() || !adv.cfg.digest_enabled {
        return;
    }
    let mut tick = tokio::time::interval(Duration::from_secs(600));
    loop {
        tick.tick().await;
        let now = Utc::now().with_timezone(&Cairo);
        if now.hour() < 7 {
            continue;
        }
        let today = now.date_naive();
        for days in due_windows(today) {
            let end = today - Days::days(1);
            let start = end - Days::days(days - 1);
            match store::digest_exists(&pool, start, end).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => {
                    log::warn!("advisor digest check failed: {e}");
                    continue;
                }
            }
            let Some(token) = mint_token(user_id) else {
                log::warn!("advisor digest: could not mint a token");
                continue;
            };
            match write(&pool, &adv, &token, user_id, days).await {
                Ok(id) => log::info!("advisor digest {start}..{end} written ({id})"),
                Err(e) => log::warn!("advisor digest {start}..{end} failed: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturdays_add_the_weekly_digest() {
        let sat = NaiveDate::from_ymd_opt(2026, 10, 24).unwrap();
        assert_eq!(sat.weekday(), Weekday::Sat);
        assert_eq!(due_windows(sat), vec![1, 7]);
        assert_eq!(due_windows(sat + Days::days(1)), vec![1]);
    }
}
