use crate::outln;
use crate::arm;
use crate::config::{self, AppConfig};
use crate::fire;
use crate::opensea;
use crate::timing;
use alloy::signers::local::PrivateKeySigner;
use eyre::{Result, WrapErr};
use std::time::Instant;

pub async fn run_public_snipe(nft: &str, qty: u64, at: i64, early_ms: i64, dry_run: bool) -> Result<()> {
    run_public_snipe_with(nft, qty, at, early_ms, dry_run, None).await
}

/// Public FCFS with optional wallet private-key override (session-selected wallet).
pub async fn run_public_snipe_with(
    nft: &str,
    qty: u64,
    at: i64,
    early_ms: i64,
    dry_run: bool,
    wallet_key: Option<&str>,
) -> Result<()> {
    if let Some(wk) = wallet_key {
        apply_wallet_override(wk)?;
    }
    let out = "armed.json";
    outln!("public arm nft={nft} qty={qty}");
    arm::arm_public(nft, qty, out).await?;
    fire::fire_armed(out, dry_run, early_ms, Some(at)).await
}

pub async fn run_api_snipe(slug: &str, qty: u64, at: i64, early_ms: i64, dry_run: bool) -> Result<()> {
    run_api_snipe_with(slug, qty, at, early_ms, dry_run, None, None).await
}

/// WL FCFS with optional wallet + OpenSea API key overrides (Telegram session map).
pub async fn run_api_snipe_with(
    slug: &str,
    qty: u64,
    at: i64,
    early_ms: i64,
    dry_run: bool,
    wallet_key: Option<&str>,
    opensea_api_key: Option<&str>,
) -> Result<()> {
    if let Some(wk) = wallet_key {
        apply_wallet_override(wk)?;
    }
    let mut cfg = AppConfig::from_env()?;
    if let Some(k) = opensea_api_key {
        let k = k.trim();
        if !k.is_empty() {
            cfg.opensea_api_key = Some(k.to_string());
        }
    }
    let api_key = cfg
        .opensea_api_key
        .clone()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| eyre::eyre!("OPENSEA_API_KEY missing — paste via Snipe Setup or set env"))?;

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
        "prewarm ok nonce={nonce} rpcs={} wallet={} — waiting until {at} (early_ms={early_ms})",
        cfg.rpc_urls.len(),
        cfg.wallet.address()
    );
    timing::sleep_until_fire(at, early_ms).await?;

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

    let t_sign = Instant::now();
    let payload = arm::sign_api_mint(&cfg, mint, slug, qty, nonce)?;
    let sign_us = t_sign.elapsed().as_secs_f64() * 1_000_000.0;
    outln!(
        "sign_eip1559_us={sign_us:.1} to={} value_wei={} hash={} nonce={nonce} (OpenSea calldata → instant sign, no estimateGas)",
        payload.seadrop,
        payload.value_wei,
        payload.tx_hash
    );

    let fire_res = fire::fire_payload(&cfg, &payload, dry_run, Some(rpc), true).await;

    let out = "armed-api.json";
    if let Ok(bytes) = serde_json::to_vec(&payload) {
        let _ = std::fs::write(out, bytes);
        outln!("armed ok file={out} hash={} (post-fire dump)", payload.tx_hash);
    }

    fire_res
}

fn apply_wallet_override(wallet_key: &str) -> Result<()> {
    let signer: PrivateKeySigner = wallet_key
        .trim()
        .parse()
        .wrap_err("invalid session wallet key")?;
    // Process-local only — never write WALLET_KEY to .env from session.
    std::env::set_var("WALLET_KEY", wallet_key.trim());
    outln!("session wallet override address={}", signer.address());
    Ok(())
}

/// Rank / doctor helpers stay on config module unchanged.
#[allow(dead_code)]
pub async fn doctor() -> Result<()> {
    config::doctor().await
}
