use crate::outln;
use crate::arm;
use crate::config;
use crate::fire;
use crate::opensea;
use crate::timing;
use eyre::{Result, WrapErr};
use std::time::Instant;

pub async fn run_public_snipe(nft: &str, qty: u64, at: i64, early_ms: i64, dry_run: bool) -> Result<()> {
    let out = "armed.json";
    outln!("public arm nft={nft} qty={qty}");
    arm::arm_public(nft, qty, out).await?;
    fire::fire_armed(out, dry_run, early_ms, Some(at)).await
}

/// WL FCFS hot path:
/// 1. Shared OpenSea HTTP/2 client + RPC prewarm + nonce cache (before countdown)
/// 2. At T-early: hammer POST /mint until first 200 with to/data/value
/// 3. Parse once → sign EIP-1559 with cached nonce (no estimateGas) → multi-RPC broadcast
/// No slow pretty-JSON dump on the critical path before broadcast.
pub async fn run_api_snipe(slug: &str, qty: u64, at: i64, early_ms: i64, dry_run: bool) -> Result<()> {
    let cfg = config::AppConfig::from_env()?;
    let api_key = cfg
        .opensea_api_key
        .clone()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| eyre::eyre!("OPENSEA_API_KEY missing — required for WL / api-snipe"))?;

    let os = opensea::http_client()?;
    let rpc = fire::rpc_http_client(cfg.rpc_urls.len())?;

    outln!("prewarm OpenSea HTTP/2 + TLS GET drop + RPCs + nonce (before countdown)");
    let (nonce_res, drop_res, _) = tokio::join!(
        arm::fetch_nonce(&cfg),
        opensea::fetch_drop_with(&os, &api_key, slug),
        fire::prewarm_rpcs(&rpc, &cfg.rpc_urls),
    );
    let nonce = nonce_res.wrap_err("prefetch nonce")?;
    match drop_res {
        Ok(details) => {
            let name = details
                .get("name")
                .or_else(|| details.get("collection_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            outln!("prewarm drop slug={slug} name={name}");
        }
        Err(e) => {
            outln!("WARN: prewarm GET /drops/{slug} failed ({e}) — connection may still be warm")
        }
    }
    outln!(
        "prewarm ok nonce={nonce} rpcs={} — waiting until {at} (early_ms={early_ms})",
        cfg.rpc_urls.len()
    );
    timing::sleep_until_fire(at, early_ms).await?;

    // Hammer only — use prewarmed nonce (no RPC on critical path).
    let t_os = Instant::now();
    let mint = opensea::hammer_until_ready_with(
        &os,
        &api_key,
        slug,
        cfg.wallet.address(),
        qty,
        opensea::hammer_timeout(),
    )
    .await?;
    let opensea_mint_ms = t_os.elapsed().as_secs_f64() * 1000.0;
    outln!("opensea_mint_ms={opensea_mint_ms:.4} (OpenSea hammer until first 200 calldata; separate from RPC fire)");

    // Hot path: parse already done → sign immediately → fire. No pretty dump first.
    let t_sign = Instant::now();
    let payload = arm::sign_api_mint(&cfg, mint, slug, qty, nonce)?;
    let sign_us = t_sign.elapsed().as_secs_f64() * 1_000_000.0;
    outln!(
        "sign_eip1559_us={sign_us:.1} to={} value_wei={} hash={} nonce={nonce} (OpenSea calldata → instant sign, no estimateGas)",
        payload.seadrop,
        payload.value_wei,
        payload.tx_hash
    );

    // Fire first; persist armed packet after (or skip write latency on critical path).
    let fire_res = fire::fire_payload(&cfg, &payload, dry_run, Some(rpc), true).await;

    // Compact write after broadcast attempt (non-critical).
    let out = "armed-api.json";
    if let Ok(bytes) = serde_json::to_vec(&payload) {
        let _ = std::fs::write(out, bytes);
        outln!("armed ok file={out} hash={} (post-fire dump)", payload.tx_hash);
    }

    fire_res
}
