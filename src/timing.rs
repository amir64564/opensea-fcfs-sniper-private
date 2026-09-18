use alloy::providers::{Provider, ProviderBuilder};
use eyre::Result;
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

pub async fn sleep_until_fire(at_unix: i64, early_ms: i64) -> Result<()> {
    let target_ms = normalize_unix_ms(at_unix) - early_ms;
    loop {
        let now = unix_now_ms();
        let left = target_ms - now;
        if left <= 0 {
            break;
        }
        // coarse then fine sleep
        if left > 50 {
            sleep(Duration::from_millis((left as u64) - 20)).await;
        } else {
            sleep(Duration::from_millis(1)).await;
        }
    }
    Ok(())
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
        .map_err(|e| eyre::eyre!("go-time parse failed ({e}): use unix or YYYY-MM-DD HH:MM[:SS] IST"))?;
    let ist = chrono::FixedOffset::east_opt(5 * 3600 + 30 * 60)
        .ok_or_else(|| eyre::eyre!("IST offset"))?;
    use chrono::TimeZone;
    let dt = ist
        .from_local_datetime(&naive)
        .single()
        .ok_or_else(|| eyre::eyre!("ambiguous IST datetime"))?;
    Ok(dt.timestamp())
}
