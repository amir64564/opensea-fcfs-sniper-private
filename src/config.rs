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

    /// Select a chain-specific RPC profile when configured.
    ///
    /// Example: BASE uses RPC_URL_BASE + BROADCAST_RPCS_BASE. If a chain profile
    /// is absent, the existing RPC_URL/BROADCAST_RPCS setup remains unchanged.
    pub async fn for_chain(&self, chain: &str) -> Result<Self> {
        let key = chain_env_key(chain);
        let rpc_key = format!("RPC_URL_{key}");
        let extra_key = format!("BROADCAST_RPCS_{key}");
        let chain_id_key = format!("CHAIN_ID_{key}");

        let Some(primary) = env::var(&rpc_key).ok().filter(|v| !v.trim().is_empty()) else {
            // Chain-specific RPC is optional. If it is not configured, reuse the normal
            // RPC_URL/BROADCAST_RPCS pool when one of those endpoints is actually on
            // the detected chain. This removes any artificial "premium RPC required"
            // dependency while still refusing to send on the wrong chain.
            if chain_matches_id(chain, self.chain_id) {
                return Ok(self.clone());
            }

            let mut fallback = self.rpc_urls.clone();
            fallback.retain(|u| !u.trim().is_empty());
            let expected = chain_id_for_name(chain);
            if !fallback.is_empty() {
                for url in &fallback {
                    let Ok(parsed_url) = url.parse() else { continue };
                    let provider = ProviderBuilder::new().on_http(parsed_url);
                    if let Ok(id) = provider.get_chain_id().await {
                            if expected.map(|x| x == id).unwrap_or(false) {
                                let mut out = self.clone();
                                out.rpc_urls = fallback;
                                out.chain_id = id;
                                crate::outln!(
                                    "multichain fallback selected chain={chain} chain_id={} rpcs={}",
                                    out.chain_id,
                                    out.rpc_urls.len()
                                );
                                return Ok(out);
                            }
                        }
                    }
                }
            }

            eyre::bail!(
                "OpenSea detected chain={chain}, but no configured RPC endpoint matches that chain. Add the chain RPC only if you actually need that chain."
            );
        };

        let mut rpc_urls = vec![primary.trim().to_string()];
        if let Ok(extra) = env::var(&extra_key) {
            for part in extra.split(',') {
                let u = part.trim();
                if !u.is_empty() && !rpc_urls.iter().any(|x| x == u) {
                    rpc_urls.push(u.to_string());
                }
            }
        }

        // Ask the selected RPC for the real chain id instead of trusting a hard-coded map.
        let provider = ProviderBuilder::new().on_http(rpc_urls[0].parse()?);
        let detected_id = provider
            .get_chain_id()
            .await
            .wrap_err_with(|| format!("detect chain id for OpenSea chain={chain}"))?;

        if let Ok(expected) = env::var(&chain_id_key) {
            let expected: u64 = expected.parse().wrap_err_with(|| format!("invalid {chain_id_key}"))?;
            if expected != detected_id {
                eyre::bail!(
                    "{rpc_key} returned chain_id={detected_id}, but {chain_id_key}={expected}"
                );
            }
        }

        let mut out = self.clone();
        out.rpc_urls = rpc_urls;
        out.chain_id = detected_id;
        crate::outln!(
            "multichain profile selected chain={chain} chain_id={} rpcs={}",
            out.chain_id,
            out.rpc_urls.len()
        );
        Ok(out)
    }
}

fn chain_env_key(chain: &str) -> String {
    chain
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn chain_id_for_name(chain: &str) -> Option<u64> {
    match chain.trim().to_ascii_lowercase().as_str() {
        "ethereum" | "mainnet" => Some(1),
        "base" => Some(8453),
        "arbitrum" | "arbitrum_one" => Some(42161),
        "optimism" => Some(10),
        "polygon" => Some(137),
        "zora" => Some(7777777),
        "blast" => Some(81457),
        "robinhood" | "robinhood_chain" => Some(4663),
        _ => None,
    }
}

fn chain_matches_id(chain: &str, chain_id: u64) -> bool {
    match chain.trim().to_ascii_lowercase().as_str() {
        "ethereum" | "mainnet" => chain_id == 1,
        "base" => chain_id == 8453,
        "arbitrum" | "arbitrum_one" => chain_id == 42161,
        "optimism" => chain_id == 10,
        "polygon" => chain_id == 137,
        "zora" => chain_id == 7777777,
        "blast" => chain_id == 81457,
        "robinhood" | "robinhood_chain" => chain_id == 4663,
        _ => false,
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
        let id = provider
            .get_chain_id()
            .await
            .wrap_err_with(|| format!("rpc {i}"))?;
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        if id != cfg.chain_id {
            return Err(eyre!(
                "rpc {i} chain_id={id} != configured {}",
                cfg.chain_id
            ));
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
        lines.push(format!(
            "rpc[{i}] {host} p50_ms={p50:.2} n={}",
            samples.len()
        ));
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

/// Lightweight startup validation (no network). Safe to call from telegram/panel.
pub fn validate_startup() -> Result<()> {
    let cfg = AppConfig::from_env()?;
    if cfg.rpc_urls.is_empty() {
        eyre::bail!("config: no RPC_URL configured");
    }
    for (i, u) in cfg.rpc_urls.iter().enumerate() {
        if !(u.starts_with("http://") || u.starts_with("https://")) {
            eyre::bail!(
                "config: rpc[{i}] must be http(s) URL (host={})",
                rpc_host_label(u)
            );
        }
    }
    if cfg.chain_id == 0 {
        eyre::bail!("config: CHAIN_ID must be non-zero");
    }
    crate::outln!(
        "config ok wallet={} chain_id={} rpcs={} opensea_key={}",
        cfg.wallet.address(),
        cfg.chain_id,
        cfg.rpc_urls.len(),
        if cfg
            .opensea_api_key
            .as_ref()
            .map(|k| !k.is_empty())
            .unwrap_or(false)
        {
            "present"
        } else {
            "absent"
        }
    );
    Ok(())
}
