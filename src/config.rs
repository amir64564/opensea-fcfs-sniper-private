use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use eyre::{eyre, Result, WrapErr};
use std::env;
use std::str::FromStr;
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub wallet: PrivateKeySigner,
    pub rpc_urls: Vec<String>,
    pub chain_id: u64,
    pub seadrop: Address,
    pub fee_recipient: Address,
    pub gas_limit: u64,
    pub max_fee_gwei: u128,
    pub priority_fee_gwei: u128,
    pub opensea_api_key: Option<String>,
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        let key = env::var("WALLET_KEY").wrap_err("WALLET_KEY missing")?;
        let wallet: PrivateKeySigner = key.parse().wrap_err("invalid WALLET_KEY")?;
        let primary = env::var("RPC_URL").wrap_err("RPC_URL missing")?;
        let mut rpc_urls = vec![primary];
        if let Ok(extra) = env::var("BROADCAST_RPCS") {
            for part in extra.split(',') {
                let u = part.trim();
                if !u.is_empty() && !rpc_urls.iter().any(|x| x == u) {
                    rpc_urls.push(u.to_string());
                }
            }
        }
        let chain_id = env::var("CHAIN_ID")
            .unwrap_or_else(|_| "4663".into())
            .parse()?;
        let seadrop = Address::from_str(
            &env::var("SEADROP_ADDRESS")
                .unwrap_or_else(|_| "0x00005EA00Ac477B1030CE78506496e8C2DE24bf5".into()),
        )?;
        // OpenSea fee recipient commonly used; override via env
        let fee_recipient = Address::from_str(
            &env::var("FEE_RECIPIENT")
                .unwrap_or_else(|_| "0x0000a26b00c1F0DF003000390027140000fAa719".into()),
        )?;
        let gas_limit = env::var("GAS_LIMIT")
            .unwrap_or_else(|_| "300000".into())
            .parse()?;
        let max_fee_gwei = env::var("MAX_FEE_GWEI")
            .unwrap_or_else(|_| "5".into())
            .parse()?;
        let priority_fee_gwei = env::var("PRIORITY_FEE_GWEI")
            .unwrap_or_else(|_| "1".into())
            .parse()?;
        let opensea_api_key = env::var("OPENSEA_API_KEY")
            .ok()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty());
        Ok(Self {
            wallet,
            rpc_urls,
            chain_id,
            seadrop,
            fee_recipient,
            gas_limit,
            max_fee_gwei,
            priority_fee_gwei,
            opensea_api_key,
        })
    }

    pub fn gwei_to_wei(gwei: u128) -> U256 {
        U256::from(gwei) * U256::from(1_000_000_000u64)
    }
}

pub async fn doctor() -> Result<()> {
    let cfg = AppConfig::from_env()?;
    crate::outln!("wallet={}", cfg.wallet.address());
    crate::outln!("chain_id={}", cfg.chain_id);
    crate::outln!("seadrop={}", cfg.seadrop);
    crate::outln!("rpcs={}", cfg.rpc_urls.len());
    for (i, url) in cfg.rpc_urls.iter().enumerate() {
        let t0 = Instant::now();
        let provider = ProviderBuilder::new().on_http(url.parse()?);
        let id = provider.get_chain_id().await.wrap_err_with(|| format!("rpc {i}"))?;
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        if id != cfg.chain_id {
            return Err(eyre!("rpc {i} chain_id={id} != configured {}", cfg.chain_id));
        }
        let bal = provider.get_balance(cfg.wallet.address()).await?;
        crate::outln!("rpc[{i}] ok latency_ms={ms:.2} balance_wei={bal}");
    }
    match &cfg.opensea_api_key {
        Some(k) => {
            crate::outln!("opensea_api_key=present chars={}", k.len());
            match crate::opensea::ping_api(k).await {
                Ok(()) => crate::outln!("opensea_api: GET /drops featured ok"),
                Err(e) => crate::outln!(
                    "WARN: opensea_api ping failed ({e}) — signed path may still work; public SeaDrop path is unaffected"
                ),
            }
        }
        None => crate::outln!(
            "WARN: OPENSEA_API_KEY missing — public SeaDrop path still works; api-arm/api-snipe need it"
        ),
    }
    crate::outln!("doctor: OK");
    Ok(())
}

/// Host-only label for an RPC URL (strip path/query/API keys).
pub fn rpc_host_label(url: &str) -> String {
    let url = url.trim();
    if url.is_empty() {
        return "[empty]".into();
    }
    let Some(scheme_end) = url.find("://") else {
        let end = url.find(['/', '?', '#']).unwrap_or(url.len());
        return url[..end].to_string();
    };
    let after = &url[scheme_end + 3..];
    let host_end = after.find(['/', '?', '#']).unwrap_or(after.len());
    format!("{}{}", &url[..scheme_end + 3], &after[..host_end])
}

pub async fn rank_rpc() -> Result<()> {
    let report = rank_rpc_report().await?;
    for line in report.lines() {
        crate::outln!("{line}");
    }
    Ok(())
}

/// Rank RPCs by eth_blockNumber p50; returns summary with hosts redacted (no full URLs).
pub async fn rank_rpc_report() -> Result<String> {
    let cfg = AppConfig::from_env()?;
    let mut rows = Vec::new();
    let mut lines = Vec::new();
    for (i, url) in cfg.rpc_urls.iter().enumerate() {
        let host = rpc_host_label(url);
        let mut samples = Vec::new();
        for _ in 0..5 {
            let t0 = Instant::now();
            let provider = ProviderBuilder::new().on_http(url.parse()?);
            match provider.get_block_number().await {
                Ok(_) => samples.push(t0.elapsed().as_secs_f64() * 1000.0),
                Err(e) => {
                    lines.push(format!("rpc[{i}] {host} FAIL {e}"));
                    samples.clear();
                    break;
                }
            }
        }
        if samples.is_empty() {
            continue;
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p50 = samples[samples.len() / 2];
        rows.push((p50, i, host.clone()));
        lines.push(format!("rpc[{i}] {host} p50_ms={p50:.2} n={}", samples.len()));
    }
    rows.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    lines.push("fastest_first:".into());
    for (p50, i, host) in &rows {
        lines.push(format!("  {p50:.2}ms rpc[{i}] {host}"));
    }
    if rows.is_empty() {
        lines.push("(no successful RPC samples)".into());
    }
    Ok(lines.join("\n"))
}
