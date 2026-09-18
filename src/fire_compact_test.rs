use crate::arm::ArmedPayload;
use crate::config::AppConfig;
use crate::timing::{sleep_until_fire, unix_now_ms};
use eyre::{Result, WrapErr};
use futures::future::join_all;
use reqwest::Client;
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::fs;
use std::time::{Duration, Instant};
use tokio::time::sleep;
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
        .timeout(Duration::from_secs(20))
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
pub async fn rank_rpc_urls(client: &Client, rpcs: &[String]) -> Vec<String> {
    if rpcs.len() <= 1 {
        return rpcs.to_vec();
    }
    let futs = rpcs.iter().enumerate().map(|(i, url)| {
        let client = client.clone();
        let url = url.clone();
        async move {
            let t0 = Instant::now();
            let body = json!({"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]});
            let ms = match client.post(&url).json(&body).send().await {
                Ok(r) => {
                    let text = r.text().await.unwrap_or_default();
                    if text.contains("\"result\"") {
                        Some(t0.elapsed().as_secs_f64() * 1000.0)
                    } else {
                        None
                    }
                }
                Err(_) => None,
            };
            (i, url, ms)
        }
    });
    let mut rows = join_all(futs).await;
    rows.sort_by(|a, b| match (a.2, b.2) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.0.cmp(&b.0),
    });
    for (i, _url, ms) in &rows {
        match ms {
            Some(ms) => crate::outln!("rpc_rank idx={i} blockNumber_ms={ms:.2}"),
            None => crate::outln!("rpc_rank idx={i} blockNumber_ms=FAIL"),
        }
    }
    rows.into_iter().map(|(_, url, _)| url).collect()
}
pub async fn fire_armed(
    armed_path: &str,
    dry_run: bool,
    early_ms: i64,
    at: Option<i64>,
) -> Result<()> {
    let cfg = AppConfig::from_env()?;
    let payload: ArmedPayload =
        serde_json::from_str(&fs::read_to_string(armed_path).wrap_err("read armed")?)?;
    if let Some(at) = at {
        crate::outln!("waiting until {at} (early_ms={early_ms})");
        sleep_until_fire(at, early_ms).await?;
    }
    fire_payload(&cfg, &payload, dry_run, None, false).await
}
pub async fn fire_payload(
    cfg: &AppConfig,
    payload: &ArmedPayload,
    dry_run: bool,
    client: Option<Client>,
    already_prewarmed: bool,
) -> Result<()> {
    let raw = payload.raw_tx_hex.clone();
    if dry_run {
        let t0 = Instant::now();
        let _ = hex::decode(raw.trim_start_matches("0x"))?;
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        crate::outln!(
            "DRY_RUN fire_local_hotpath_ms={ms:.4} tx_hash={} rpcs={}",
            payload.tx_hash,
            cfg.rpc_urls.len()
        );
        return Ok(());
    }
    let client = match client {
        Some(c) => c,
        None => rpc_http_client(cfg.rpc_urls.len())?,
    };
    if !already_prewarmed {
        prewarm_rpcs(&client, &cfg.rpc_urls).await;
    }
    let rpc_urls = if rpc_auto_rank_enabled() {
        crate::outln!("RPC_AUTO_RANK=1 — measuring eth_blockNumber before broadcast (parallel rank)");
        rank_rpc_urls(&client, &cfg.rpc_urls).await
    } else {
        cfg.rpc_urls.clone()
    };
    let t0 = Instant::now();
    let fire_start_ms = unix_now_ms();
    let first = broadcast_all(&client, &rpc_urls, &raw).await;
    if let Some(ms) = first.first_ok_ms {
        crate::outln!(
            "fire_broadcast_ms={ms:.4} started_unix_ms={fire_start_ms} (first RPC result)"
        );
    } else {
        let elapsed = t0.elapsed().as_secs_f64() * 1000.0;
        crate::outln!(
            "fire_broadcast_ms={elapsed:.4} started_unix_ms={fire_start_ms} (no RPC returned result)"
        );
    }
    for line in &first.logs {
        crate::outln!("{line}");
    }
    if first.any_ok {
        crate::outln!("submitted tx_hash={}", payload.tx_hash);
    } else {
        crate::outln!(
            "initial broadcast failed on all RPCs — will still try inclusion/RBF path if configured"
        );
    }
    let watch_ms = std::env::var("INCLUSION_WATCH_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let rbf_after = std::env::var("RBF_AFTER_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let rbf_max = std::env::var("RBF_MAX")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let probe = rpc_urls
        .first()
        .cloned()
        .unwrap_or_else(|| cfg.rpc_urls[0].clone());
    if watch_ms > 0 {
        let deadline = Instant::now() + Duration::from_millis(watch_ms);
        let mut polls = 0u32;
        while Instant::now() < deadline {
            polls += 1;
            sleep(Duration::from_millis(50)).await;
            if receipt_exists(&client, &probe, &payload.tx_hash).await? {
                let inclusion_ms = t0.elapsed().as_secs_f64() * 1000.0;
                crate::outln!("inclusion_ms={inclusion_ms:.4} polls={polls}");
                return Ok(());
            }
        }
        crate::outln!(
            "inclusion_ms=timeout after {watch_ms}ms polls={polls} (receipt still null)"
        );
    } else if rbf_after > 0 && rbf_max > 0 {
        crate::outln!("note: RBF_AFTER_MS set — same nonce requires re-arm with higher PRIORITY_FEE_GWEI / MAX_FEE_GWEI then fire again");
        for i in 1..=rbf_max {
            sleep(Duration::from_millis(rbf_after)).await;
            if receipt_exists(&client, &probe, &payload.tx_hash).await? {
                let inclusion_ms = t0.elapsed().as_secs_f64() * 1000.0;
                crate::outln!("inclusion_ms={inclusion_ms:.4} after bump-wait #{i}");
                return Ok(());
            }
            crate::outln!("still pending after {}ms (wait #{i}/{rbf_max})", rbf_after);
        }
    }
    if !first.any_ok {
        eyre::bail!("all RPC broadcasts failed");
    }
    Ok(())
}
struct BroadcastResult {
    any_ok: bool,
    first_ok_ms: Option<f64>,
    logs: Vec<String>,
}
async fn broadcast_all(client: &Client, rpcs: &[String], raw: &str) -> BroadcastResult {
    let futs = rpcs.iter().enumerate().map(|(i, url)| {
        let client = client.clone();
        let url = url.clone();
        let raw = raw.to_string();
        async move {
            let t_rpc = Instant::now();
            let body = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_sendRawTransaction",
                "params": [raw],
            });
            match client.post(&url).json(&body).send().await {
                Ok(r) => {
                    let status = r.status();
                    let text = r.text().await.unwrap_or_default();
                    let ok = text.contains("\"result\"") && !text.contains("\"error\"");
                    let ok = ok || text.contains("already known") || text.contains("nonce too low");
                    let ms = t_rpc.elapsed().as_secs_f64() * 1000.0;
                    (
                        ok,
                        ms,
                        format!(
                            "rpc[{i}] broadcast_ms={ms:.4} status={status} body={}",
                            truncate(&text, 120)
                        ),
                    )
                }
                Err(e) => {
                    let ms = t_rpc.elapsed().as_secs_f64() * 1000.0;
                    (false, ms, format!("rpc[{i}] broadcast_ms={ms:.4} err={e}"))
                }
            }
        }
    });
    let results = join_all(futs).await;
    let mut logs = Vec::new();
    let mut any_ok = false;
    let mut first_ok_ms: Option<f64> = None;
    for (ok, ms, line) in results {
        if ok {
            any_ok = true;
            first_ok_ms = Some(match first_ok_ms {
                Some(prev) => prev.min(ms),
                None => ms,
            });
        }
        logs.push(line);
    }
    BroadcastResult {
        any_ok,
        first_ok_ms,
        logs,
    }
}
async fn receipt_exists(client: &Client, rpc: &str, tx_hash: &str) -> Result<bool> {
    let body = json!({
        "jsonrpc":"2.0","id":1,
        "method":"eth_getTransactionReceipt",
        "params":[tx_hash]
    });
    let text = client.post(rpc).json(&body).send().await?.text().await?;
    let v: Value = serde_json::from_str(&text).unwrap_or(json!({}));
    Ok(!v.get("result").map(|r| r.is_null()).unwrap_or(true))
}
fn truncate(s: &str, n: usize) -> &str {
    if s.len() <= n { s } else { &s[..n] }
}
