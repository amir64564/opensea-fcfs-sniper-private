use crate::arm;
use crate::config::{self, AppConfig};
use crate::fire;
use crate::opensea;
use crate::outln;
use crate::timing;
use alloy::signers::local::PrivateKeySigner;
use eyre::{Result, WrapErr};
use std::time::Instant;

pub async fn run_public_snipe(
    nft: &str,
    qty: u64,
    at: Option<i64>,
    early_ms: i64,
    dry_run: bool,
) -> Result<()> {
    run_public_snipe_with(nft, qty, at, early_ms, dry_run, None).await
}

/// Public FCFS with optional wallet private-key override (session-selected wallet).
/// `at = None` → auto-detect from on-chain `getPublicDrop.startTime`.
pub async fn run_public_snipe_with(
    nft: &str,
    qty: u64,
    at: Option<i64>,
    early_ms: i64,
    dry_run: bool,
    wallet_key: Option<&str>,
) -> Result<()> {
    if let Some(wk) = wallet_key {
        apply_wallet_override(wk)?;
    }
    let at = match at {
        Some(t) => {
            outln!(
                "go-time override at={t} ({}) (explicit)",
                timing::format_go_time_zones(t)
            );
            t
        }
        None => {
            let t = arm::public_drop_start_unix(nft).await?;
            outln!(
                "auto-time public getPublicDrop startTime={t} ({})",
                timing::format_go_time_zones(t)
            );
            t
        }
    };
    let out = "armed.json";
    outln!("public arm nft={nft} qty={qty}");
    arm::arm_public(nft, qty, out).await?;
    // Prep before fire window: load signed packet, prewarm + rank, THEN wait → broadcast.
    let mut cfg = AppConfig::from_env()?;
    let payload: arm::ArmedPayload =
        serde_json::from_str(&std::fs::read_to_string(out).wrap_err("read armed")?)?;
    let rpc = fire::rpc_http_client(cfg.rpc_urls.len())?;
    fire::prewarm_rpcs(&rpc, &cfg.rpc_urls).await;
    if std::env::var("RPC_AUTO_RANK")
        .map(|v| {
            !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        })
        .unwrap_or(true)
        && cfg.rpc_urls.len() > 1
    {
        cfg.rpc_urls = fire::rank_rpc_urls(&rpc, &cfg.rpc_urls).await;
        outln!(
            "rpc rank applied before public countdown ({} endpoints)",
            cfg.rpc_urls.len()
        );
    }
    outln!(
        "public prep ready — waiting until {} (early_ms={early_ms})",
        timing::format_go_time_zones(at)
    );
    timing::sleep_until_fire(at, early_ms).await?;
    let _ = fire::fire_payload(&cfg, &payload, dry_run, Some(rpc), true).await?;
    Ok(())
}

pub async fn run_api_snipe(
    slug: &str,
    qty: u64,
    at: Option<i64>,
    early_ms: i64,
    dry_run: bool,
) -> Result<()> {
    run_api_snipe_with(slug, qty, at, early_ms, dry_run, None, None).await
}

/// WL FCFS with optional wallet + OpenSea API key overrides (Telegram session map).
/// `at = None` → auto-detect from OpenSea drop details (active/next stage startTime).
pub async fn run_api_snipe_with(
    slug: &str,
    qty: u64,
    at: Option<i64>,
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

    let at = match at {
        Some(t) => {
            if let Ok(details) = &drop_res {
                let name = details
                    .get("name")
                    .or_else(|| details.get("collection_name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                outln!("prewarm drop slug={slug} name={name}");
            } else if let Err(e) = &drop_res {
                outln!(
                    "WARN: prewarm GET /drops/{slug} failed ({e}) — connection may still be warm"
                );
            }
            outln!(
                "go-time override at={t} ({}) (explicit)",
                timing::format_go_time_zones(t)
            );
            t
        }
        None => {
            let details = drop_res.wrap_err("GET drop required for auto-time")?;
            let name = details
                .get("name")
                .or_else(|| details.get("collection_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            outln!("prewarm drop slug={slug} name={name}");
            let stage = opensea::pick_relevant_stage_start(&details)?;
            outln!(
                "auto-time stage source={} type={} label={} startTime={}",
                stage.source,
                stage.stage_type,
                stage.label,
                stage.start_unix
            );
            stage.start_unix
        }
    };

    outln!(
        "prewarm ok nonce={nonce} rpcs={} wallet={} — waiting until {} (early_ms={early_ms})",
        cfg.rpc_urls.len(),
        cfg.wallet.address(),
        timing::format_go_time_zones(at)
    );
    // Rank RPCs during wait window (not on fire hot path). Failures stay in fan-out.
    if std::env::var("RPC_AUTO_RANK")
        .map(|v| {
            !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        })
        .unwrap_or(true)
        && cfg.rpc_urls.len() > 1
    {
        let ranked = fire::rank_rpc_urls(&rpc, &cfg.rpc_urls).await;
        cfg.rpc_urls = ranked;
        outln!(
            "rpc rank applied before countdown ({} endpoints)",
            cfg.rpc_urls.len()
        );
    }

    // Start OpenSea hammer near T (lead window), not hours early — avoids 429 burn.
    // When calldata lands before fire time, sign early; hotpath = wait → broadcast.
    let hammer_lead_ms: i64 = std::env::var("OPENSEA_HAMMER_LEAD_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3_000);
    // Sleep until (T - early_ms - lead), cancelable; then begin hammer.
    let lead_early = early_ms.saturating_add(hammer_lead_ms);
    outln!(
        "opensea hammer lead_ms={hammer_lead_ms} (start before fire window; final hotpath = wait → single-fire → broadcast)"
    );
    timing::sleep_until_fire(at, lead_early).await?;

    let os_h = os.clone();
    let api_key_h = api_key.clone();
    let slug_h = slug.to_string();
    let minter = cfg.wallet.address();
    let qty_h = qty;
    let hammer_timeout = opensea::hammer_timeout();
    let mut hammer = tokio::spawn(async move {
        opensea::hammer_until_ready_with(&os_h, &api_key_h, &slug_h, minter, qty_h, hammer_timeout)
            .await
    });

    let early_mint = tokio::select! {
        res = &mut hammer => {
            Some(res.map_err(|e| eyre::eyre!("opensea hammer task join: {e}"))?)
        }
        wait_res = timing::sleep_until_fire(at, early_ms) => {
            wait_res?;
            None
        }
    };

    let payload = match early_mint {
        Some(mint_res) => {
            let mint = mint_res?;
            outln!(
                "opensea calldata ready before fire — signing early; hotpath = wait → broadcast"
            );
            let t_sign = Instant::now();
            let payload = arm::sign_api_mint(&cfg, mint, slug, qty, nonce)?;
            let sign_us = t_sign.elapsed().as_secs_f64() * 1_000_000.0;
            outln!(
                "sign_eip1559_us={sign_us:.1} to={} value_wei={} hash={} nonce={nonce} (early arm)",
                payload.seadrop,
                payload.value_wei,
                payload.tx_hash
            );
            // sleep_until_fire is idempotent if already past target
            timing::sleep_until_fire(at, early_ms).await?;
            payload
        }
        None => {
            outln!("fire window open — finishing OpenSea hammer if still pending");
            let t_os = Instant::now();
            let mint = hammer
                .await
                .map_err(|e| eyre::eyre!("opensea hammer task join: {e}"))??;
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
            payload
        }
    };

    let fire_out = fire::fire_payload(&cfg, &payload, dry_run, Some(rpc), true).await?;

    let out = "armed-api.json";
    if let Ok(bytes) = serde_json::to_vec(&payload) {
        let _ = std::fs::write(out, bytes);
        outln!(
            "armed ok file={out} hash={} (post-fire dump)",
            payload.tx_hash
        );
    }

    outln!(
        "snipe_wl done tx={} rpc={} broadcast_ms={:?} inclusion_ms={:?} dry={}",
        fire_out.tx_hash,
        fire_out.first_ok_rpc.as_deref().unwrap_or("-"),
        fire_out.first_ok_ms,
        fire_out.inclusion_ms,
        fire_out.dry_run
    );
    Ok(())
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
