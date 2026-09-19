use crate::config::AppConfig;
use crate::opensea::{self, MintTx};
use crate::seadrop::{encode_mint_public_drop, ISeaDrop};
use alloy::consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy::eips::eip2718::Encodable2718;
use alloy::primitives::{Address, Bytes, TxKind, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::SignerSync;
use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};
use std::fs;
use std::str::FromStr;

#[derive(Debug, Serialize, Deserialize)]
pub struct ArmedPayload {
    pub chain_id: u64,
    pub nft: String,
    pub seadrop: String,
    pub fee_recipient: String,
    pub from: String,
    pub qty: u64,
    pub mint_price_wei: String,
    pub value_wei: String,
    pub nonce: u64,
    pub gas_limit: u64,
    pub max_fee_per_gas: String,
    pub max_priority_fee_per_gas: String,
    pub raw_tx_hex: String,
    pub tx_hash: String,
    pub armed_at_unix_ms: i64,
    pub drop_start: u64,
    pub drop_end: u64,
}

pub async fn arm_public(nft: &str, qty: u64, out: &str) -> Result<()> {
    let cfg = AppConfig::from_env()?;
    let nft = Address::from_str(nft).wrap_err("nft address")?;
    let provider = ProviderBuilder::new().on_http(cfg.rpc_urls[0].parse()?);

    let seadrop = ISeaDrop::new(cfg.seadrop, provider.clone());
    let drop = seadrop.getPublicDrop(nft).call().await?._0;
    let mint_price: U256 = U256::from(drop.mintPrice);
    let value = mint_price * U256::from(qty);
    let calldata = encode_mint_public_drop(nft, cfg.fee_recipient, cfg.wallet.address(), qty);

    write_armed(
        &cfg,
        cfg.seadrop,
        calldata,
        value,
        mint_price,
        format!("{nft}"),
        qty,
        drop.startTime.to::<u64>(),
        drop.endTime.to::<u64>(),
        out,
    )
    .await?;
    Ok(())
}

/// On-chain SeaDrop public stage startTime (unix seconds).
pub async fn public_drop_start_unix(nft: &str) -> Result<i64> {
    let cfg = AppConfig::from_env()?;
    let nft = Address::from_str(nft).wrap_err("nft address")?;
    let provider = ProviderBuilder::new().on_http(cfg.rpc_urls[0].parse()?);
    let seadrop = ISeaDrop::new(cfg.seadrop, provider);
    let drop = seadrop.getPublicDrop(nft).call().await?._0;
    let start = drop.startTime.to::<u64>() as i64;
    if start == 0 {
        eyre::bail!("getPublicDrop startTime is 0 — public drop not configured?");
    }
    Ok(start)
}

/// Hammer OpenSea Drops API for signed/WL mint calldata, then sign EIP-1559 locally (no estimateGas).
pub async fn arm_api(slug: &str, qty: u64, out: &str) -> Result<()> {
    let cfg = AppConfig::from_env()?;
    let api_key = cfg
        .opensea_api_key
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| eyre::eyre!("OPENSEA_API_KEY missing — required for api-arm / api-snipe"))?;

    let client = opensea::http_client()?;
    match opensea::fetch_drop_with(&client, api_key, slug).await {
        Ok(details) => {
            let name = details
                .get("name")
                .or_else(|| details.get("collection_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            crate::outln!("opensea drop slug={slug} name={name}");
        }
        Err(e) => crate::outln!("WARN: GET /drops/{slug} failed ({e}) — continuing to mint hammer"),
    }

    let mint: MintTx = opensea::hammer_until_ready_with(
        &client,
        api_key,
        slug,
        cfg.wallet.address(),
        qty,
        opensea::hammer_timeout(),
    )
    .await?;

    let mint_price = if qty > 0 {
        mint.value / U256::from(qty)
    } else {
        mint.value
    };
    if let Some(chain) = &mint.chain {
        crate::outln!(
            "opensea mint chain={chain} to={} value_wei={}",
            mint.to,
            mint.value
        );
    } else {
        crate::outln!("opensea mint to={} value_wei={}", mint.to, mint.value);
    }

    write_armed(
        &cfg,
        mint.to,
        mint.data,
        mint.value,
        mint_price,
        format!("slug:{slug}"),
        qty,
        0,
        0,
        out,
    )
    .await?;
    Ok(())
}

pub async fn fetch_nonce(cfg: &AppConfig) -> Result<u64> {
    // Pending nonce: includes mempool txs so we do not reuse a stale "latest" nonce.
    // Fetched during prep only — never on the final-ms hotpath.
    let provider = ProviderBuilder::new().on_http(cfg.rpc_urls[0].parse()?);
    Ok(provider
        .get_transaction_count(cfg.wallet.address())
        .pending()
        .await?)
}

/// Sign EIP-1559 with a known nonce. No RPC, no estimateGas — hot path after calldata lands.
pub fn sign_call(
    cfg: &AppConfig,
    to: Address,
    input: Bytes,
    value: U256,
    mint_price: U256,
    nft_label: String,
    qty: u64,
    drop_start: u64,
    drop_end: u64,
    nonce: u64,
) -> Result<ArmedPayload> {
    let max_fee = AppConfig::gwei_to_wei(cfg.max_fee_gwei);
    let tip = AppConfig::gwei_to_wei(cfg.priority_fee_gwei);

    let tx = TxEip1559 {
        chain_id: cfg.chain_id,
        nonce,
        gas_limit: cfg.gas_limit,
        max_fee_per_gas: max_fee.try_into().unwrap_or(u128::MAX),
        max_priority_fee_per_gas: tip.try_into().unwrap_or(u128::MAX),
        to: TxKind::Call(to),
        value,
        input,
        access_list: Default::default(),
    };

    let tx = tx;
    let sig = cfg
        .wallet
        .sign_hash_sync(&tx.signature_hash())
        .wrap_err("sign")?;
    let signed = tx.into_signed(sig);
    let envelope: TxEnvelope = signed.into();
    let raw = envelope.encoded_2718();
    let hash = *envelope.tx_hash();

    Ok(ArmedPayload {
        chain_id: cfg.chain_id,
        nft: nft_label,
        seadrop: format!("{to}"),
        fee_recipient: format!("{}", cfg.fee_recipient),
        from: format!("{}", cfg.wallet.address()),
        qty,
        mint_price_wei: mint_price.to_string(),
        value_wei: value.to_string(),
        nonce,
        gas_limit: cfg.gas_limit,
        max_fee_per_gas: max_fee.to_string(),
        max_priority_fee_per_gas: tip.to_string(),
        raw_tx_hex: format!("0x{}", hex::encode(&raw)),
        tx_hash: format!("{hash}"),
        armed_at_unix_ms: chrono::Utc::now().timestamp_millis(),
        drop_start,
        drop_end,
    })
}

pub fn sign_api_mint(
    cfg: &AppConfig,
    mint: MintTx,
    slug: &str,
    qty: u64,
    nonce: u64,
) -> Result<ArmedPayload> {
    let mint_price = if qty > 0 {
        mint.value / U256::from(qty)
    } else {
        mint.value
    };
    sign_call(
        cfg,
        mint.to,
        mint.data,
        mint.value,
        mint_price,
        format!("slug:{slug}"),
        qty,
        0,
        0,
        nonce,
    )
}

pub async fn write_armed(
    cfg: &AppConfig,
    to: Address,
    input: Bytes,
    value: U256,
    mint_price: U256,
    nft_label: String,
    qty: u64,
    drop_start: u64,
    drop_end: u64,
    out: &str,
) -> Result<ArmedPayload> {
    let nonce = fetch_nonce(cfg).await?;
    let payload = sign_call(
        cfg, to, input, value, mint_price, nft_label, qty, drop_start, drop_end, nonce,
    )?;
    fs::write(out, serde_json::to_string_pretty(&payload)?)?;
    crate::outln!(
        "armed ok file={out} hash={} nonce={nonce} value_wei={value} start={drop_start} end={drop_end}",
        payload.tx_hash
    );
    Ok(payload)
}
