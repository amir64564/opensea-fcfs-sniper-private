use crate::arm::ArmedPayload;
use crate::config::{self, AppConfig};
use crate::timing::{sleep_until_fire, unix_now_ms};
use eyre::{Result, WrapErr};
use futures::future::join_all;
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
    let futs = rpcs.iter().enumerate().map(|(i, url)| {
        let client = client.clone();
        let url = url.clone();
        async move {
            let t0 = Instant::now();
            let body = json!({"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]});
            let ms = match tokio::time::timeout(
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
    for (i, url, ms) in &rows {
        let host = config::rpc_host_label(url);
        match ms {
            Some(ms) => crate::outln!("rpc_rank idx={i} host={host} blockNumber_ms={ms:.2}"),
            None => crate::outln!("rpc_rank idx={i} host={host} blockNumber_ms=FAIL"),
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
    let _ = fire_armed_detailed(armed_path, dry_run, early_ms, at).await?;
    Ok(())
}

pub async fn fire_armed_detailed(
    armed_path: &str,
    dry_run: bool,
    early_ms: i64,
    at: Option<i64>,
) -> Result<FireOutcome> {
    let cfg = AppConfig::from_env()?;
    let payload: ArmedPayload =
        serde_json::from_str(&fs::read_to_string(armed_path).wrap_err("read armed")?)?;

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

    fire_payload(&cfg, &payload, dry_run, None, false).await
}

/// Broadcast an already-signed packet. No estimateGas.
/// If `already_prewarmed`, skip RPC keep-alive AND skip hot-path re-rank
/// (rank during prewarm instead — bad RPC must not add latency at fire).
pub async fn fire_payload(
    cfg: &AppConfig,
    payload: &ArmedPayload,
    dry_run: bool,
    client: Option<Client>,
    already_prewarmed: bool,
) -> Result<FireOutcome> {
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
        return Ok(FireOutcome {
            tx_hash: payload.tx_hash.clone(),
            any_ok: true,
            first_ok_ms: Some(ms),
            first_ok_rpc: None,
            inclusion_ms: None,
            dry_run: true,
        });
    }

    let client = match client {
        Some(c) => c,
        None => rpc_http_client(cfg.rpc_urls.len())?,
    };

    // Prewarm (and optional rank) only when NOT on the WL hot path.
    // When already_prewarmed: connections are warm; ranking was done (or skipped) earlier.
    let rpc_urls = if already_prewarmed {
        cfg.rpc_urls.clone()
    } else {
        prewarm_rpcs(&client, &cfg.rpc_urls).await;
        if rpc_auto_rank_enabled() {
            crate::outln!("RPC_AUTO_RANK=1 — ranking before broadcast (not on WL hot path)");
            rank_rpc_urls(&client, &cfg.rpc_urls).await
        } else {
            cfg.rpc_urls.clone()
        }
    };

    // Drop nothing — even "failed" rank entries stay in fan-out so a recovered RPC can still mint.
    let t0 = Instant::now();
    let fire_start_ms = unix_now_ms();
    let first = broadcast_all(&client, &rpc_urls, &raw).await;

    if let Some(ms) = first.first_ok_ms {
        let host = first
            .first_ok_rpc
            .as_deref()
            .map(config::rpc_host_label)
            .unwrap_or_else(|| "?".into());
        crate::outln!(
            "fire_broadcast_ms={ms:.4} rpc={host} started_unix_ms={fire_start_ms} (first RPC ok)"
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

    let probe = first
        .first_ok_rpc
        .clone()
        .or_else(|| rpc_urls.first().cloned())
        .unwrap_or_else(|| cfg.rpc_urls[0].clone());

    let mut inclusion_ms = None;
    if watch_ms > 0 {
        let deadline = Instant::now() + Duration::from_millis(watch_ms);
        let mut polls = 0u32;
        while Instant::now() < deadline {
            polls += 1;
            sleep(Duration::from_millis(50)).await;
            match receipt_exists(&client, &probe, &payload.tx_hash).await {
                Ok(true) => {
                    let ms = t0.elapsed().as_secs_f64() * 1000.0;
                    crate::outln!("inclusion_ms={ms:.4} polls={polls}");
                    inclusion_ms = Some(ms);
                    break;
                }
                Ok(false) => {}
                Err(e) => {
                    // Bad probe RPC must not fail the mint after a successful broadcast.
                    crate::outln!("inclusion probe err (ignored): {e}");
                }
            }
        }
        if inclusion_ms.is_none() {
            crate::outln!(
                "inclusion_ms=timeout after {watch_ms}ms polls={polls} (receipt still null)"
            );
        }
    } else if rbf_after > 0 && rbf_max > 0 {
        crate::outln!("note: RBF_AFTER_MS set — same nonce requires re-arm with higher PRIORITY_FEE_GWEI / MAX_FEE_GWEI then fire again");
        for i in 1..=rbf_max {
            sleep(Duration::from_millis(rbf_after)).await;
            if matches!(
                receipt_exists(&client, &probe, &payload.tx_hash).await,
                Ok(true)
            ) {
                let ms = t0.elapsed().as_secs_f64() * 1000.0;
                crate::outln!("inclusion_ms={ms:.4} after bump-wait #{i}");
                inclusion_ms = Some(ms);
                break;
            }
            crate::outln!("still pending after {}ms (wait #{i}/{rbf_max})", rbf_after);
        }
    }

    if !first.any_ok {
        eyre::bail!("all RPC broadcasts failed");
    }
    Ok(FireOutcome {
        tx_hash: payload.tx_hash.clone(),
        any_ok: first.any_ok,
        first_ok_ms: first.first_ok_ms,
        first_ok_rpc: first.first_ok_rpc.map(|u| config::rpc_host_label(&u)),
        inclusion_ms,
        dry_run: false,
    })
}

struct BroadcastResult {
    any_ok: bool,
    first_ok_ms: Option<f64>,
    first_ok_rpc: Option<String>,
    logs: Vec<String>,
}

async fn broadcast_all(client: &Client, rpcs: &[String], raw: &str) -> BroadcastResult {
    let futs = rpcs.iter().enumerate().map(|(i, url)| {
        let client = client.clone();
        let url = url.clone();
        let raw = raw.to_string();
        async move {
            let host = config::rpc_host_label(&url);
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
                        url,
                        format!(
                            "rpc[{i}] host={host} broadcast_ms={ms:.4} status={status} body={}",
                            truncate(&text, 120)
                        ),
                    )
                }
                Err(e) => {
                    let ms = t_rpc.elapsed().as_secs_f64() * 1000.0;
                    (
                        false,
                        ms,
                        url,
                        format!("rpc[{i}] host={host} broadcast_ms={ms:.4} err={e}"),
                    )
                }
            }
        }
    });
    let results = join_all(futs).await;
    let mut logs = Vec::new();
    let mut any_ok = false;
    let mut first_ok_ms: Option<f64> = None;
    let mut first_ok_rpc: Option<String> = None;
    for (ok, ms, url, line) in results {
        if ok {
            any_ok = true;
            match first_ok_ms {
                Some(prev) if ms < prev => {
                    first_ok_ms = Some(ms);
                    first_ok_rpc = Some(url);
                }
                None => {
                    first_ok_ms = Some(ms);
                    first_ok_rpc = Some(url);
                }
                _ => {}
            }
        }
        logs.push(line);
    }
    BroadcastResult {
        any_ok,
        first_ok_ms,
        first_ok_rpc,
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
    if s.len() <= n {
        s
    } else {
        &s[..n]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_works() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdef", 3), "abc");
    }

    #[test]
    fn fire_outcome_default_not_ok() {
        let o = FireOutcome::default();
        assert!(!o.any_ok);
        assert!(!o.dry_run);
    }
}
