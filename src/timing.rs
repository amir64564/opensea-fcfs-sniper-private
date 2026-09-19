use alloy::providers::{Provider, ProviderBuilder};
use eyre::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::time::sleep;

pub fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Accept unix seconds (~1e9) or unix milliseconds (~1e12).
pub fn normalize_unix_ms(at: i64) -> i64 {
    if at.abs() >= 1_000_000_000_000 {
        at
    } else {
        at.saturating_mul(1000)
    }
}

/// Format unix seconds for logs: UTC + IST (Asia/Kolkata, UTC+5:30).
pub fn format_go_time_zones(unix_secs: i64) -> String {
    use chrono::{FixedOffset, TimeZone, Utc};
    let utc = Utc
        .timestamp_opt(unix_secs, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| format!("{unix_secs}"));
    let ist = FixedOffset::east_opt(5 * 3600 + 30 * 60)
        .and_then(|off| off.timestamp_opt(unix_secs, 0).single())
        .map(|d| d.format("%Y-%m-%d %H:%M:%S IST").to_string())
        .unwrap_or_default();
    if ist.is_empty() {
        utc
    } else {
        format!("{utc} / {ist}")
    }
}

pub async fn sleep_until_fire(at_unix: i64, early_ms: i64) -> Result<()> {
    // Honor process-global cancel (Telegram Cancel Session) when set.
    let reached =
        sleep_until_fire_cancelable(at_unix, early_ms, Some(crate::task::global_cancel_flag()))
            .await?;
    if !reached {
        eyre::bail!("cancelled during countdown");
    }
    Ok(())
}

/// Sleep until (at - early_ms). Returns Ok(true) if reached, Ok(false) if cancelled.
pub async fn sleep_until_fire_cancelable(
    at_unix: i64,
    early_ms: i64,
    cancel: Option<&AtomicBool>,
) -> Result<bool> {
    let target_ms = normalize_unix_ms(at_unix) - early_ms;
    crate::outln!(
        "countdown target_fire_ms={target_ms} early_ms={early_ms} go_at={}",
        format_go_time_zones(if at_unix.abs() >= 1_000_000_000_000 {
            at_unix / 1000
        } else {
            at_unix
        })
    );
    loop {
        if cancel.map(|c| c.load(Ordering::Acquire)).unwrap_or(false) {
            crate::outln!("countdown cancelled");
            return Ok(false);
        }
        let now = unix_now_ms();
        let left = target_ms - now;
        if left <= 0 {
            break;
        }
        // coarse then fine sleep; check cancel ~every 200ms on long waits
        if left > 50 {
            let chunk = ((left as u64) - 20).min(200);
            sleep(Duration::from_millis(chunk)).await;
        } else {
            sleep(Duration::from_millis(1)).await;
        }
    }
    Ok(true)
}

/// Rough RPC latency helper (ms).
pub async fn rpc_latency_ms(rpc: &str) -> Result<f64> {
    let provider = ProviderBuilder::new().on_http(rpc.parse()?);
    let t0 = Instant::now();
    let _ = provider.get_block_number().await?;
    Ok(t0.elapsed().as_secs_f64() * 1000.0)
}

/// Unix seconds/ms, or IST/local datetime (`YYYY-MM-DD HH:MM[:SS]` or with `T`).
/// Naive datetimes are treated as Asia/Kolkata (UTC+5:30).
pub fn parse_go_time(s: &str) -> Result<i64> {
    let t = s.trim();
    if t.is_empty() {
        eyre::bail!("empty go-time");
    }
    if let Ok(n) = t.parse::<i64>() {
        return Ok(n);
    }
    let cleaned = t.replace('T', " ").replace("+05:30", "").replace("IST", "");
    let cleaned = cleaned.trim();
    let naive = chrono::NaiveDateTime::parse_from_str(cleaned, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(cleaned, "%Y-%m-%d %H:%M"))
        .map_err(|e| {
            eyre::eyre!("go-time parse failed ({e}): use unix or YYYY-MM-DD HH:MM[:SS] IST")
        })?;
    let ist = chrono::FixedOffset::east_opt(5 * 3600 + 30 * 60)
        .ok_or_else(|| eyre::eyre!("IST offset"))?;
    use chrono::TimeZone;
    let dt = ist
        .from_local_datetime(&naive)
        .single()
        .ok_or_else(|| eyre::eyre!("ambiguous IST datetime"))?;
    Ok(dt.timestamp())
}

/// Explicit `--at` wins. `None` / `"auto"` / `--auto-time` → auto-detect (`Ok(None)`).
pub fn resolve_at_arg(at: Option<&str>, _auto_time: bool) -> Result<Option<i64>> {
    // Explicit --at wins. Omit / "auto" / --auto-time → None (caller auto-detects).
    if let Some(s) = at {
        let t = s.trim();
        if !t.is_empty() && !t.eq_ignore_ascii_case("auto") {
            return Ok(Some(parse_go_time(t)?));
        }
    }
    Ok(None)
}

/// Telegram / free-text: treat missing, empty, or `auto` as auto-detect.
pub fn resolve_at_token(tok: Option<&str>) -> Result<Option<i64>> {
    match tok.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(t) if t.eq_ignore_ascii_case("auto") => Ok(None),
        Some(t) => Ok(Some(parse_go_time(t)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_secs_vs_ms() {
        assert_eq!(normalize_unix_ms(1_700_000_000), 1_700_000_000_000);
        assert_eq!(normalize_unix_ms(1_700_000_000_000), 1_700_000_000_000);
    }

    #[test]
    fn explicit_at_wins_over_auto_flag() {
        let v = resolve_at_arg(Some("1700000000"), true).unwrap();
        assert_eq!(v, Some(1_700_000_000));
        let auto = resolve_at_arg(Some("auto"), false).unwrap();
        assert_eq!(auto, None);
        let omitted = resolve_at_arg(None, true).unwrap();
        assert_eq!(omitted, None);
    }

    #[test]
    fn ist_naive_datetime_parses() {
        // 2024-01-01 05:30 IST = 2024-01-01 00:00 UTC
        let t = parse_go_time("2024-01-01 05:30").unwrap();
        assert_eq!(t, 1_704_067_200); // 2024-01-01T00:00:00Z?
                                      // 2024-01-01 05:30 IST = 2024-01-01 00:00 UTC = 1704067200
        assert_eq!(t, 1_704_067_200);
    }

    #[test]
    fn format_includes_ist_label() {
        let s = format_go_time_zones(1_704_067_200);
        assert!(s.contains("UTC"));
        assert!(s.contains("IST"));
    }

    #[tokio::test]
    async fn cancelable_sleep_aborts() {
        let flag = AtomicBool::new(false);
        let far = unix_now_ms() / 1000 + 3600;
        let h = tokio::spawn(async move {
            // can't move ref — use owned Arc in real code; here test immediate cancel path
        });
        drop(h);
        flag.store(true, Ordering::Release);
        let reached = sleep_until_fire_cancelable(far, 0, Some(&flag))
            .await
            .unwrap();
        assert!(!reached);
    }
}
