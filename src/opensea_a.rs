use alloy::primitives::{Address, Bytes, U256};
use eyre::{eyre, Result, WrapErr};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use std::str::FromStr;
use std::time::{Duration, Instant};
use tokio::task::JoinSet;
use tokio::time::sleep;

const OPENSEA_API: &str = "https://api.opensea.io";

#[derive(Debug, Clone)]
pub struct MintTx {
    pub to: Address,
    pub data: Bytes,
    pub value: U256,
    pub chain: Option<String>,
}

/// Shared persistent OpenSea HTTP client (HTTP/2, TCP_NODELAY, keep-alive).
/// One Client for the whole process so TLS + H2 connections stay warm across prewarm → hammer.
pub fn http_client() -> Result<Client> {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<Client> = OnceLock::new();
    if let Some(c) = CLIENT.get() {
        return Ok(c.clone());
    }
    let built = Client::builder()
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(8)
        .http2_adaptive_window(true)
        .http2_keep_alive_interval(Duration::from_secs(10))
        .http2_keep_alive_timeout(Duration::from_secs(10))
        .http2_keep_alive_while_idle(true)
        .timeout(Duration::from_secs(15))
        .build()?;
    let _ = CLIENT.set(built.clone());
    Ok(CLIENT.get().cloned().unwrap_or(built))
}

pub fn hammer_timeout() -> Duration {
    let ms = std::env::var("OPENSEA_HAMMER_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60_000u64);
    Duration::from_millis(ms)
}

pub fn hammer_backoff_ms() -> u64 {
    std::env::var("OPENSEA_HAMMER_BACKOFF_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15)
}

pub fn hammer_parallel() -> usize {
    std::env::var("OPENSEA_HAMMER_PARALLEL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2usize)
        .clamp(1, 4)
}

/// POST /api/v2/drops/{slug}/mint — one shot.
pub async fn build_drop_mint(
    api_key: &str,
    slug: &str,
    minter: Address,
    qty: u64,
) -> Result<MintTx> {
    let client = http_client()?;
    build_drop_mint_with(&client, api_key, slug, minter, qty).await
}

pub async fn build_drop_mint_with(
    client: &Client,
    api_key: &str,
    slug: &str,
    minter: Address,
    qty: u64,
) -> Result<MintTx> {
    let (status, body) = post_mint(client, api_key, slug, minter, qty).await?;
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        eyre::bail!("OpenSea API auth failed status={status} — check OPENSEA_API_KEY");
    }
    if !status.is_success() {
        eyre::bail!("OpenSea mint HTTP {status}: {}", truncate(&body, 240));
    }
    parse_mint_tx(&body).wrap_err("parse mint tx from 200 body")
}

pub async fn hammer_until_ready(
    api_key: &str,
    slug: &str,
    minter: Address,
    qty: u64,
    timeout: Duration,
) -> Result<MintTx> {
    let client = http_client()?;
    hammer_until_ready_with(&client, api_key, slug, minter, qty, timeout).await
}

/// Loop parallel POST until 200 with tx data or timeout.
///
/// Retry: 429 / 5xx / 409 / 422 / not-ready-ish 4xx.
/// Fail fast: 401 / 403 / 400 / 404.
pub async fn hammer_until_ready_with(
    client: &Client,
    api_key: &str,
    slug: &str,
    minter: Address,
    qty: u64,
    timeout: Duration,
) -> Result<MintTx> {
    let t0 = Instant::now();
    let mut wave: u32 = 0;
    let mut consec_429: u32 = 0;
    let mut consec_5xx: u32 = 0;
    let backoff_ms = hammer_backoff_ms();
    let parallel = hammer_parallel();
    // Log sparsely: first response, status changes, every LOG_EVERY waves, READY/fatal.
    const LOG_EVERY: u32 = 25;
    let mut last_logged_status: Option<u16> = None;
    let mut logged_first = false;
    crate::outln!(
        "opensea hammer slug={slug} parallel={parallel} backoff_ms={backoff_ms} timeout_ms={}",
        timeout.as_millis()
    );

    loop {
        if t0.elapsed() >= timeout {
            eyre::bail!(
                "OpenSea mint hammer timed out after {}ms ({wave} waves) slug={slug}",
                t0.elapsed().as_millis()
            );
        }
        wave += 1;

        let mut set = JoinSet::new();
        for i in 0..parallel {
            let client = client.clone();
            let api_key = api_key.to_string();
            let slug = slug.to_string();
            set.spawn(async move {
                let r = post_mint(&client, &api_key, &slug, minter, qty).await;
                (i, r)
            });
        }

        let mut saw_retry = false;
        let mut last_retry_status: u16 = 0;
        let mut wave_status: Option<u16> = None;
        let mut wave_snip = String::new();
        while let Some(joined) = set.join_next().await {
            let (i, result) = match joined {
                Ok(v) => v,
                Err(e) => {
                    if wave == 1 || wave % LOG_EVERY == 0 {
                        crate::outln!("opensea mint wave={wave} join_err={e}");
                    }
                    saw_retry = true;
                    continue;
                }
            };
            match result {
                Ok((status, body)) => {
                    let code = status.as_u16();
                    if wave_status.is_none() {
                        wave_status = Some(code);
                        wave_snip = truncate(&body, 100).to_string();
                    }
                    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                        set.abort_all();
                        eyre::bail!(
                            "OpenSea API auth failed status={status} — check OPENSEA_API_KEY"
                        );
                    }
                    if status.is_success() {
                        match parse_mint_tx(&body) {
                            Ok(tx) => {
                                set.abort_all();
                                crate::outln!(
                                    "opensea mint READY wave={wave} worker={i} elapsed_ms={} to={}",
                                    t0.elapsed().as_millis(),
                                    tx.to
                                );
                                return Ok(tx);
                            }
                            Err(e) => {
                                crate::outln!("opensea mint 200 but missing tx fields: {e}");
                                saw_retry = true;
                            }
                        }
                    } else if code == 400 || code == 404 {
                        set.abort_all();
                        eyre::bail!(
                            "OpenSea mint fatal status={code} body={}",
                            truncate(&body, 240)
                        );
                    } else if is_retryable(status, &body) {
                        saw_retry = true;
                        last_retry_status = code;
                    } else {
                        set.abort_all();
                        eyre::bail!(
                            "OpenSea mint non-retryable status={code} body={}",
                            truncate(&body, 240)
                        );
                    }
                }
                Err(e) => {
                    if wave == 1 || wave % LOG_EVERY == 0 {
                        crate::outln!("opensea mint wave={wave} worker={i} transport_err={e}");
                    }
                    saw_retry = true;
                }
            }
        }

        if let Some(code) = wave_status {
            let status_changed = last_logged_status != Some(code);
            let milestone = wave == 1 || wave % LOG_EVERY == 0;
            if !logged_first || status_changed || milestone {
                crate::outln!(
                    "opensea mint wave={wave} status={code} elapsed_ms={} body={}",
                    t0.elapsed().as_millis(),
                    wave_snip
                );
                logged_first = true;
                last_logged_status = Some(code);
            }
        }

        if !saw_retry {
            eyre::bail!("OpenSea mint hammer: empty wave with no retryable result");
        }
        // Adaptive backoff: 429/5xx back off carefully; not-started stays snappy.
        // Never raise parallel — that worsens 429s.
        let sleep_ms = if last_retry_status == 429 {
            consec_429 = consec_429.saturating_add(1);
            consec_5xx = 0;
            let exp = 50u64.saturating_mul(1u64 << consec_429.min(4));
            backoff_ms.max(exp).min(1_000)
        } else if (500..600).contains(&last_retry_status) {
            consec_5xx = consec_5xx.saturating_add(1);
            consec_429 = 0;
            let exp = backoff_ms.saturating_mul(1u64 << consec_5xx.min(3));
            exp.min(500).max(backoff_ms)
        } else {
            // not-started / 409 / 422 — keep base backoff, reset rate-limit streak
            consec_429 = 0;
            consec_5xx = 0;
            backoff_ms
        };
        sleep(Duration::from_millis(sleep_ms)).await;
    }
}

pub async fn fetch_drop(api_key: &str, slug: &str) -> Result<Value> {
    let client = http_client()?;
    fetch_drop_with(&client, api_key, slug).await
}

/// GET /api/v2/drops/{slug} — details + HTTP/2 prewarm.
pub async fn fetch_drop_with(client: &Client, api_key: &str, slug: &str) -> Result<Value> {
    let url = format!("{OPENSEA_API}/api/v2/drops/{slug}");
    let resp = client
        .get(&url)
        .header("X-API-KEY", api_key)
        .header("Accept", "application/json")
        .send()
        .await
        .wrap_err("GET drop")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        eyre::bail!("GET drop status={status} body={}", truncate(&text, 200));
    }
    serde_json::from_str(&text).wrap_err("drop json")
}

pub async fn ping_api(api_key: &str) -> Result<()> {
    let client = http_client()?;
    ping_api_with(&client, api_key).await
}

pub async fn ping_api_with(client: &Client, api_key: &str) -> Result<()> {
    let resp = client
        .get(format!("{OPENSEA_API}/api/v2/drops?type=featured&limit=1"))
        .header("X-API-KEY", api_key)
        .header("Accept", "application/json")
        .send()
        .await
        .wrap_err("GET featured drops")?;
    let status = resp.status();
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        eyre::bail!("auth failed status={status}");
    }
    if !status.is_success() {
        eyre::bail!("status={status}");
    }
    Ok(())
}

