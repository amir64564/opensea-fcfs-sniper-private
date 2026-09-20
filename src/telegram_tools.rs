use crate::{arm, config, fire, opensea, session};
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol;
use alloy::sol_types::SolCall;
use eyre::{Result, WrapErr};
use reqwest::StatusCode;
use serde_json::Value;
use std::str::FromStr;

sol! {
    interface ITelegramNFTTools {
        function transferFrom(address from, address to, uint256 tokenId);
        function safeTransferFrom(address from, address to, uint256 tokenId);
        function burn(uint256 tokenId);
        function safeTransferFrom(address from, address to, uint256 id, uint256 amount, bytes data);
    }
}

fn parse_addr(s: &str) -> Result<Address> {
    Address::from_str(s.trim()).wrap_err("invalid address")
}

fn parse_hex_bytes(s: &str) -> Result<Bytes> {
    let raw = s.trim().strip_prefix("0x").unwrap_or(s.trim());
    if raw.is_empty() {
        return Ok(Bytes::new());
    }
    if raw.len() % 2 != 0 {
        eyre::bail!("hex calldata must have an even number of characters");
    }
    Ok(Bytes::from(hex::decode(raw).wrap_err("invalid hex calldata")?))
}

pub fn parse_eth(s: &str) -> Result<U256> {
    let s = s.trim();
    if s.is_empty() {
        eyre::bail!("ETH amount is empty");
    }
    let mut it = s.split('.');
    let whole = it.next().unwrap_or("0");
    let frac = it.next().unwrap_or("");
    if it.next().is_some() || !whole.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        eyre::bail!("invalid ETH amount: {s}");
    }
    if frac.len() > 18 {
        eyre::bail!("ETH amount supports max 18 decimals");
    }
    let digits = format!("{}{}", if whole.is_empty() { "0" } else { whole }, format!("{:0<18}", frac));
    U256::from_str(&digits).wrap_err("ETH amount overflow")
}

pub fn main_menu() -> Value {
    serde_json::json!({
        "keyboard": [
            [{"text":"🎨 Mint NFT"},{"text":"🎯 Snipe"}],
            [{"text":"📦 Batch Mint"},{"text":"🎯 Batch Snipe"}],
            [{"text":"🔧 Manual Mint"},{"text":"🎛️ Exec"}],
            [{"text":"🎯 My Snipes"},{"text":"❌ Cancel Session"}],
            [{"text":"🔐 Snipe Setup"},{"text":"⚡ Arm"}],
            [{"text":"📤 Send NFTs"},{"text":"📤 Batch Send"}],
            [{"text":"🔥 Burn NFTs"},{"text":"🏦 Consolidate"}],
            [{"text":"🔍 Eligibility"},{"text":"💸 Disperse ETH"}],
            [{"text":"💸 Send ETH"},{"text":"👛 Wallets"}],
            [{"text":"🌐 RPC"},{"text":"⚙️ Settings"}],
            [{"text":"/status"},{"text":"/help"}]
        ],
        "resize_keyboard": true,
        "is_persistent": true
    })
}

pub fn feature_help() -> String {
    r#"Feature commands:
🎨 Mint NFT
  /mint <slug> <qty>
🎯 Snipe
  /snipe_wl <slug> <qty> [at|auto] [early_ms] [dry]
📦 Batch Mint
  /batch_mint <slug:qty,slug:qty,...>
🎯 Batch Snipe
  /batch_snipe <slug:qty:at,slug:qty:at,...>
🔧 Manual Mint
  /manual_mint <contract> <value_eth> <calldata_hex> CONFIRM
🎛️ Exec
  /exec <to> <value_eth> <calldata_hex> CONFIRM
📤 Send NFTs
  /send_nft <erc721|erc1155> ...
📤 Batch Send
  /batch_send <erc721|erc1155> ...
🔥 Burn NFTs
  /burn <contract> <token_id> CONFIRM
🏦 Consolidate
  /consolidate <target> [wallets|all] CONFIRM
🔍 Eligibility
  /eligibility <slug> [wallet_index]
💸 Disperse ETH
  /disperse_eth <amount_each_eth> <target1,target2,...> CONFIRM
💸 Send ETH
  /send_eth <to> <amount_eth> CONFIRM

Generic transaction buttons use CONFIRM so a typo cannot immediately send funds."#.into()
}

pub fn my_snipes() -> String {
    let mut out = String::from("My Snipes\n");
    let mut found = false;
    for (label, path) in [("public", "armed.json"), ("wl", "armed-api.json")] {
        if let Ok(raw) = std::fs::read_to_string(path) {
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                found = true;
                let hash = v.get("tx_hash").and_then(Value::as_str).unwrap_or("-");
                let nonce = v.get("nonce").and_then(Value::as_u64).unwrap_or_default();
                let nft = v.get("nft").and_then(Value::as_str).unwrap_or("-");
                out.push_str(&format!("{label}: hash={hash} nonce={nonce} target={nft}\n"));
            }
        }
    }
    if !found {
        out.push_str("No armed transaction files found.");
    }
    out
}

pub async fn mint(api_key: &str, wallet: &session::WalletEntry, slug: &str, qty: u64) -> Result<String> {
    let cfg = config_for_wallet(wallet)?;
    let client = opensea::http_client()?;
    let mint = opensea::build_drop_mint_with(&client, api_key, slug, wallet.address, qty).await?;
    let nonce = arm::fetch_nonce(&cfg).await?;
    let payload = arm::sign_api_mint(&cfg, mint, slug, qty, nonce)?;
    let out = fire::fire_payload(
        &cfg,
        &payload,
        false,
        Some(fire::rpc_http_client(cfg.rpc_urls.len())?),
        false,
    ).await?;
    Ok(format!("Mint OK\nslug={slug} qty={qty}\ntx={}\nrpc={}", out.tx_hash, out.first_ok_rpc.as_deref().unwrap_or("-")))
}

pub async fn batch_mint(api_key: &str, wallet: &session::WalletEntry, items: &[(String, u64)]) -> Result<String> {
    let mut lines = Vec::new();
    for (slug, qty) in items {
        match mint(api_key, wallet, slug, *qty).await {
            Ok(s) => lines.push(s),
            Err(e) => lines.push(format!("FAIL slug={slug}: {}", crate::errclass::sanitize(&format!("{e:#}")))),
        }
    }
    Ok(lines.join("\n\n"))
}

pub async fn batch_snipe(api_key: &str, wallet: &session::WalletEntry, items: &[(String, u64, Option<i64>)], early_ms: i64, dry: bool) -> Result<String> {
    let mut lines = Vec::new();
    for (slug, qty, at) in items {
        match crate::ops::run_api_snipe_with(slug, *qty, *at, early_ms, dry, Some(wallet.private_key.clone()), Some(api_key.to_string())).await {
            Ok(()) => lines.push(format!("OK {slug} qty={qty} at={at:?}")),
            Err(e) => lines.push(format!("FAIL {slug}: {}", crate::errclass::sanitize(&format!("{e:#}")))),
        }
    }
    Ok(lines.join("\n"))
}

fn config_for_wallet(wallet: &session::WalletEntry) -> Result<config::AppConfig> {
    let mut cfg = config::AppConfig::from_env()?;
    cfg.wallet = wallet.private_key.parse().wrap_err("wallet key")?;
    Ok(cfg)
}

async fn execute_raw(wallet: &session::WalletEntry, to: Address, value: U256, data: Bytes, label: String) -> Result<String> {
    let cfg = config_for_wallet(wallet)?;
    let nonce = arm::fetch_nonce(&cfg).await?;
    let payload = arm::sign_call(&cfg, to, data, value, value, label, 1, 0, 0, nonce)?;
    let out = fire::fire_payload(
        &cfg,
        &payload,
        false,
        Some(fire::rpc_http_client(cfg.rpc_urls.len())?),
        false,
    ).await?;
    Ok(format!("tx={} rpc={} broadcast_ms={:?}", out.tx_hash, out.first_ok_rpc.as_deref().unwrap_or("-"), out.first_ok_ms))
}

pub async fn exec(wallet: &session::WalletEntry, to: &str, value_eth: &str, data_hex: &str) -> Result<String> {
    let to = parse_addr(to)?;
    let value = parse_eth(value_eth)?;
    let data = parse_hex_bytes(data_hex)?;
    execute_raw(wallet, to, value, data, "telegram-exec".into()).await
}

pub async fn manual_mint(wallet: &session::WalletEntry, contract: &str, value_eth: &str, calldata: &str) -> Result<String> {
    let to = parse_addr(contract)?;
    let value = parse_eth(value_eth)?;
    let data = parse_hex_bytes(calldata)?;
    execute_raw(wallet, to, value, data, "telegram-manual-mint".into()).await
}

pub async fn send_eth(wallet: &session::WalletEntry, to: &str, amount_eth: &str) -> Result<String> {
    exec(wallet, to, amount_eth, "0x").await
}

pub async fn send_nft_erc721(wallet: &session::WalletEntry, contract: &str, to: &str, token_id: u64, safe: bool) -> Result<String> {
    let contract = parse_addr(contract)?;
    let to = parse_addr(to)?;
    let data = if safe {
        ITelegramNFTTools::safeTransferFrom_0Call {
            from: wallet.address,
            to,
            tokenId: U256::from(token_id),
        }.abi_encode()
    } else {
        ITelegramNFTTools::transferFromCall {
            from: wallet.address,
            to,
            tokenId: U256::from(token_id),
        }.abi_encode()
    };
    execute_raw(wallet, contract, U256::ZERO, Bytes::from(data), format!("telegram-erc721-{token_id}")).await
}

pub async fn send_nft_erc1155(wallet: &session::WalletEntry, contract: &str, to: &str, token_id: u64, amount: u64) -> Result<String> {
    let contract = parse_addr(contract)?;
    let to = parse_addr(to)?;
    let data = ITelegramNFTTools::safeTransferFrom_1Call {
        from: wallet.address,
        to,
        id: U256::from(token_id),
        amount: U256::from(amount),
        data: Bytes::new(),
    }.abi_encode();
    execute_raw(wallet, contract, U256::ZERO, Bytes::from(data), format!("telegram-erc1155-{token_id}")).await
}

pub async fn burn_erc721(wallet: &session::WalletEntry, contract: &str, token_id: u64) -> Result<String> {
    let contract = parse_addr(contract)?;
    let data = ITelegramNFTTools::burnCall { tokenId: U256::from(token_id) }.abi_encode();
    execute_raw(wallet, contract, U256::ZERO, Bytes::from(data), format!("telegram-burn-{token_id}")).await
}

pub async fn eligibility(api_key: &str, wallet: &session::WalletEntry, slug: &str, qty: u64) -> Result<String> {
    let client = opensea::http_client()?;
    let url = format!("https://api.opensea.io/api/v2/drops/{slug}/mint");
    let body = serde_json::json!({"minter": format!("{}", wallet.address), "quantity": qty.max(1)});
    let resp = client.post(url)
        .header("X-API-KEY", api_key)
        .header("Accept", "application/json")
        .json(&body)
        .send().await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.is_success() {
        return Ok(format!("ELIGIBLE / mint request accepted\nwallet={}\nslug={slug} qty={qty}", wallet.address));
    }
    if status == StatusCode::UNPROCESSABLE_ENTITY {
        return Ok(format!("NOT READY / precondition failed\nwallet={}\nslug={slug} qty={qty}\nOpenSea returned 422: wallet may be ineligible, over limit, insufficient balance, or supply may be exhausted.", wallet.address));
    }
    Ok(format!("Eligibility check HTTP {status}\n{}", &text[..text.len().min(300)]))
}

pub async fn consolidate(wallets: &[session::WalletEntry], target: Address) -> Result<String> {
    let mut lines = Vec::new();
    for w in wallets {
        let cfg = config_for_wallet(w)?;
        let provider = ProviderBuilder::new().on_http(cfg.rpc_urls[0].parse()?);
        let bal = provider.get_balance(w.address).await?;
        let gas_limit = U256::from(21_000u64);
        let max_fee = config::AppConfig::gwei_to_wei(cfg.max_fee_gwei);
        let reserve = gas_limit * max_fee;
        if bal <= reserve {
            lines.push(format!("SKIP {} balance too low after gas reserve", session::display_wallet(w)));
            continue;
        }
        let amount = bal - reserve;
        let mut send_cfg = cfg.clone();
        send_cfg.gas_limit = 21_000;
        let nonce = arm::fetch_nonce(&send_cfg).await?;
        let payload = arm::sign_call(&send_cfg, target, Bytes::new(), amount, U256::ZERO, "telegram-consolidate".into(), 1, 0, 0, nonce)?;
        match fire::fire_payload(&send_cfg, &payload, false, Some(fire::rpc_http_client(send_cfg.rpc_urls.len())?), false).await {
            Ok(out) => lines.push(format!("OK {} amount_wei={} tx={}", session::display_wallet(w), amount, out.tx_hash)),
            Err(e) => lines.push(format!("FAIL {}: {}", session::display_wallet(w), crate::errclass::sanitize(&format!("{e:#}")))),
        }
    }
    Ok(lines.join("\n"))
}

pub async fn disperse_eth(wallet: &session::WalletEntry, amount_eth: &str, targets: &[Address]) -> Result<String> {
    let amount = parse_eth(amount_eth)?;
    let mut lines = Vec::new();
    for target in targets {
        match send_eth(wallet, &format!("{target}"), amount_eth).await {
            Ok(s) => lines.push(format!("{target}: {s}")),
            Err(e) => lines.push(format!("{target}: FAIL {}", crate::errclass::sanitize(&format!("{e:#}")))),
        }
    }
    let _ = amount;
    Ok(lines.join("\n"))
}
