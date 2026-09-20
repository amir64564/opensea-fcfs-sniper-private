use crate::arm::ArmedPayload;
use crate::config::{self, AppConfig};
use crate::timing::{sleep_until_fire, unix_now_ms};
use eyre::{Result, WrapErr};
use futures::future::join_all;
use futures::stream::{FuturesUnordered, StreamExt};
use reqwest::Client;
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::fs;
use std::time::{Duration, Instant};
use tokio::time::sleep;

#[derive(Debug, Clone, Default)]
pub struct FireOutcome {
    pub tx_hash: String,
    pub any_ok: bool,
    pub first_ok_ms: Option<f64>,
    pub first_ok_rpc: Option<String>,
    pub inclusion_ms: Option<f64>,
    pub dry_run: bool,
}

pub fn rpc_http_client(pool: usize) -> Result<Client> {
    Ok(Client::builder()
        .pool_max_idle_per_host(pool.max(4))
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(90))
        .http2_adaptive_window(true)
        .http2_keep_alive_interval(Duration::from_secs(15))
        .http2_keep_alive_timeout(Duration::from_secs(10))
        .http2_keep_alive_while_idle(true)
        // Per-request timeout: bad RPC must not stall the whole fan-out forever.
        .timeout(Duration::from_secs(8))
        .build()?)
}

pub async fn prewarm_rpcs(client: &Client, rpcs: &[String]) {
    let warm = rpcs.iter().map(|url| {
        let client = client.clone();
        let url = url.clone();
        async move {
            let _ = client
                .post(&url)
                .json(&json!({"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}))
                .send()
                .await;
        }
    });
    join_all(warm).await;
}

fn rpc_auto_rank_enabled() -> bool {
    match std::env::var("RPC_AUTO_RANK") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !(v == "0" || v == "false" || v == "no" || v == "off")
        }
        Err(_) => true, // default on
    }
}

/// Parallel eth_blockNumber probe; return URLs sorted fastest-first. Failures last.
/// Ordering only affects fan-out list / inclusion probe — broadcast stays parallel.
pub async fn rank_rpc_urls(client: &Client, rpcs: &[String]) -> Vec<String> {
    if rpcs.len() <= 1 {
        return rpcs.to_vec();
    }
    // Multi-sample median + failure demotion. Rank BEFORE hotpath only.
    let samples_n: usize = std::env::var("RPC_RANK_SAMPLES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3usize)
        .clamp(1, 5);
    let futs = rpcs.iter().enumerate().map(|(i, url)| {
        let client = client.clone();
        let url = url.clone();
        async move {
            let mut samples: Vec<f64> = Vec::with_capacity(samples_n);
            let mut fails: u32 = 0;
            for _ in 0..samples_n {
                let t0 = Instant::now();
                let body = json!({"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]});
                let ok_ms = match tokio::time::timeout(
                    Duration::from_millis(1500),
                    client.post(&url).json(&body).send(),
                )
                .await
                {
                    Ok(Ok(r)) => {
                        let text = r.text().await.unwrap_or_default();
                        if text.contains("\"result\"") {
                            Some(t0.elapsed().as_secs_f64() * 1000.0)
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                match ok_ms {
                    Some(ms) => samples.push(ms),
                    None => fails += 1,
                }
            }
            let median = if samples.is_empty() {
                None
            } else {
                samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
                Some(samples[samples.len() / 2])
            };
            (i, url, median, fails)
        }
    });
    let mut rows = join_all(futs).await;
    rows.sort_by(|a, b| match (a.2, b.2) {
        (Some(x), Some(y)) => match x.partial_cmp(&y).unwrap_or(Ordering::Equal) {
            Ordering::Equal => a.3.cmp(&b.3).then_with(|| a.0.cmp(&b.0)),
            o => o,
        },
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.3.cmp(&b.3).then_with(|| a.0.cmp(&b.0)),
    });
    for (i, url, ms, fails) in &rows {
        let host = config::rpc_host_label(url);
        match ms {
            Some(ms) => crate::outln!(
                "rpc_rank idx={i} host={host} blockNumber_p50_ms={ms:.2} fails={fails}/{samples_n}"
            ),
            None => crate::outln!(
                "rpc_rank idx={i} host={host} blockNumber_p50_ms=FAIL fails={fails}/{samples_n}"
            ),
        }
    }
    rows.into_iter().map(|(_, url, _, _)| url).collect()
}

pub async fn fire_armed(
    armed_path: &str,
    dry_run: bool,
    early_ms: i64,
    at: Option<i64>,
) -> Result<()> {
    let _ = fire_armed_detailed(armed_path, dry_run, early_ms, at).await?;
    Ok(())
}

pub async fn fire_armed_detailed(
    armed_path: &str,
    dry_run: bool,
    early_ms: i64,
    at: Option<i64>,
) -> Result<FireOutcome> {
    let mut cfg = AppConfig::from_env()?;
    let payload: ArmedPayload =
        serde_json::from_str(&fs::read_to_string(armed_path).wrap_err("read armed")?)?;

    let client = rpc_http_client(cfg.rpc_urls.len())?;
    // Timed fire: prewarm + rank BEFORE wait so hotpath is broadcast-only.
    let already_prewarmed = if at.is_some() {
        prewarm_rpcs(&client, &cfg.rpc_urls).await;
        if rpc_auto_rank_enabled() && cfg.rpc_urls.len() > 1 {
            crate::outln!("RPC_AUTO_RANK=1 — ranking before countdown (not on fire hot path)");
            cfg.rpc_urls = rank_rpc_urls(&client, &cfg.rpc_urls).await;
        }
        true
    } else {
        false
    };

    if let Some(at) = at {
        crate::outln!(
            "waiting until {} (early_ms={early_ms})",
            crate::timing::format_go_time_zones(if at.abs() >= 1_000_000_000_000 {
                at / 1000
            } else {
                at
            })
        );
        sleep_until_fire(at, early_ms).await?;
    }

    fire_payload(&cfg, &payload, dry_run, Some(client), already_prewarmed).await
}
