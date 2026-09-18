//! Telegram long-poll control bot (reqwest). Never logs WALLET_KEY / private keys.
use crate::config;
use crate::ops;
use eyre::{Result, WrapErr};
use reqwest::Client;
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::path::Path;
use std::time::Duration;

pub async fn run() -> Result<()> {
    let token = env::var("TELEGRAM_BOT_TOKEN")
        .wrap_err("TELEGRAM_BOT_TOKEN missing — create a bot via @BotFather and set env")?;
    let allow_chat = env::var("TELEGRAM_CHAT_ID")
        .wrap_err("TELEGRAM_CHAT_ID missing — only this chat may control the sniper")?
        .trim()
        .to_string();
    if token.is_empty() || allow_chat.is_empty() {
        eyre::bail!("TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID must be non-empty");
    }

    let client = Client::builder()
        .timeout(Duration::from_secs(90))
        .tcp_nodelay(true)
        .build()?;
    let api = format!("https://api.telegram.org/bot{token}");

    // Identity check (do not log token)
    let me: Value = client
        .get(format!("{api}/getMe"))
        .send()
        .await?
        .json()
        .await?;
    if me.get("ok") != Some(&json!(true)) {
        eyre::bail!("Telegram getMe failed (bad TELEGRAM_BOT_TOKEN?)");
    }
    let uname = me["result"]["username"].as_str().unwrap_or("?");
    crate::outln!("telegram bot online @{uname} allow_chat_id={allow_chat}");
    let _ = send(
        &client,
        &api,
        &allow_chat,
        "sniper telegram online. /help for commands. Private keys are never logged.",
    )
    .await;

    let mut offset: i64 = 0;
    loop {
        let url = format!("{api}/getUpdates?timeout=50&offset={offset}");
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                crate::outln!("telegram poll transport_err={e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let body: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                crate::outln!("telegram poll json_err={e}");
                continue;
            }
        };
        if body.get("ok") != Some(&json!(true)) {
            crate::outln!("telegram getUpdates not ok");
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        let Some(updates) = body.get("result").and_then(|v| v.as_array()) else {
            continue;
        };
        for u in updates {
            if let Some(id) = u.get("update_id").and_then(|v| v.as_i64()) {
                offset = id + 1;
            }
            let msg = match u.get("message").or_else(|| u.get("edited_message")) {
                Some(m) => m,
                None => continue,
            };
            let chat_id = msg
                .get("chat")
                .and_then(|c| c.get("id"))
                .map(|v| v.to_string().trim_matches('"').to_string())
                .unwrap_or_default();
            if chat_id != allow_chat {
                crate::outln!("telegram ignore unauthorized chat_id={chat_id}");
                continue;
            }
            let text = msg
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if text.is_empty() {
                continue;
            }
            // Strip @botname from /cmd@bot
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let reply = match handle_command(&text).await {
                Ok(s) => s,
                Err(e) => format!("error: {e:#}"),
            };
            // Redact accidental key-looking hex blobs longer than 60 chars in replies
            let safe = redact_secrets(&reply);
            if let Err(e) = send(&client, &api, &allow_chat, &safe).await {
                crate::outln!("telegram send_err={e}");
            }
        }
    }
}

fn redact_secrets(s: &str) -> String {
    // Never echo env private keys; also scrub 64+ hex runs that look like keys.
    let mut cleaned = s.replace("WALLET_KEY", "[redacted]");
    let bytes = cleaned.clone().into_bytes();
    let mut i = 0;
    let mut rebuilt = String::with_capacity(bytes.len());
    while i < bytes.len() {
        if i + 66 <= bytes.len()
            && bytes[i] == b'0'
            && bytes[i + 1] == b'x'
            && bytes[i + 2..i + 66]
                .iter()
                .all(|c| c.is_ascii_hexdigit())
        {
            rebuilt.push_str("0x[redacted_key]");
            i += 66;
        } else {
            rebuilt.push(bytes[i] as char);
            i += 1;
        }
    }
    cleaned = rebuilt;
    if let Ok(tok) = env::var("TELEGRAM_BOT_TOKEN") {
        if !tok.is_empty() {
            cleaned = cleaned.replace(&tok, "[redacted_token]");
        }
    }
    cleaned
}


/// Rewrite or append KEY=value in ./.env. Never prints the value. Also set_var for this process.
fn update_env_key(key: &str, value: &str) -> Result<()> {
    let path = Path::new(".env");
    let content = if path.exists() {
        fs::read_to_string(path).wrap_err("read .env")?
    } else {
        String::new()
    };
    let prefix = format!("{key}=");
    let mut found = false;
    let mut out_lines: Vec<String> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        if !found && trimmed.starts_with(&prefix) && !trimmed.starts_with('#') {
            out_lines.push(format!("{key}={value}"));
            found = true;
        } else {
            out_lines.push(line.to_string());
        }
    }
    if !found {
        out_lines.push(format!("{key}={value}"));
    }
    let mut out = out_lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    fs::write(path, out).wrap_err("write .env")?;
    env::set_var(key, value);
    Ok(())
}

fn collect_rpc_urls_from_env() -> Vec<String> {
    let mut urls = Vec::new();
    if let Ok(primary) = env::var("RPC_URL") {
        let u = primary.trim();
        if !u.is_empty() {
            urls.push(u.to_string());
        }
    }
    if let Ok(extra) = env::var("BROADCAST_RPCS") {
        for part in extra.split(',') {
            let u = part.trim();
            if !u.is_empty() && !urls.iter().any(|x| x == u) {
                urls.push(u.to_string());
            }
        }
    }
    urls
}

fn list_rpcs_text() -> String {
    let urls = collect_rpc_urls_from_env();
    if urls.is_empty() {
        return "rpcs: (none — set /rpc_set <url>)".into();
    }
    let mut lines = vec![format!("rpcs: {} (host only; keys redacted)", urls.len())];
    for (i, url) in urls.iter().enumerate() {
        let role = if i == 0 { "primary" } else { "broadcast" };
        lines.push(format!("[{i}] {role} {}", config::rpc_host_label(url)));
    }
    lines.join("\n")
}

async fn send(client: &Client, api: &str, chat_id: &str, text: &str) -> Result<()> {
    // Telegram message limit ~4096
    let chunk = if text.len() > 4000 {
        format!("{}…", &text[..4000])
    } else {
        text.to_string()
    };
    let body = json!({
        "chat_id": chat_id,
        "text": chunk,
        "disable_web_page_preview": true,
    });
    let r = client
        .post(format!("{api}/sendMessage"))
        .json(&body)
        .send()
        .await?;
    if !r.status().is_success() {
        let t = r.text().await.unwrap_or_default();
        eyre::bail!("sendMessage failed: {}", &t[..t.len().min(200)]);
    }
    Ok(())
}

async fn handle_command(text: &str) -> Result<String> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.is_empty() {
        return Ok("empty".into());
    }
    let cmd = parts[0].split('@').next().unwrap_or(parts[0]).to_lowercase();
    match cmd.as_str() {
        "/help" | "help" => Ok(help_text()),
        "/doctor" => {
            // Capture doctor via running same checks briefly
            match capture_doctor().await {
                Ok(s) => Ok(s),
                Err(e) => Err(e),
            }
        }
        "/status" => Ok(status_text()),
        "/panel_hint" => Ok(
            "Local panel: ./target/release/opensea-fcfs-sniper panel\nOpen http://127.0.0.1:8787 (bind is localhost-only). Shows wallet address, never the private key."
                .into(),
        ),
        "/rpc" => Ok(list_rpcs_text()),
        "/rpc_set" => {
            if parts.len() < 2 {
                return Ok("usage: /rpc_set <url>".into());
            }
            let url = parts[1..].join(" ");
            let url = url.trim();
            if url.is_empty() || !url.contains("://") {
                return Ok("usage: /rpc_set <url> (must include scheme, e.g. https://…)".into());
            }
            update_env_key("RPC_URL", url)?;
            // Do not echo URL/secrets — confirm only + reload note.
            Ok(format!(
                "saved\nRPC_URL updated (host {}). Process env reloaded for this telegram session; restart other binaries to pick up .env.",
                config::rpc_host_label(url)
            ))
        }
        "/rpc_add" => {
            if parts.len() < 2 {
                return Ok("usage: /rpc_add <url>".into());
            }
            let url = parts[1..].join(" ");
            let url = url.trim();
            if url.is_empty() || !url.contains("://") {
                return Ok("usage: /rpc_add <url> (must include scheme, e.g. https://…)".into());
            }
            let mut extras: Vec<String> = env::var("BROADCAST_RPCS")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if extras.iter().any(|x| x == url) {
                return Ok(format!(
                    "already present (host {}). No change.",
                    config::rpc_host_label(url)
                ));
            }
            if let Ok(primary) = env::var("RPC_URL") {
                if primary.trim() == url {
                    return Ok(format!(
                        "url is already RPC_URL primary (host {}). Not adding to BROADCAST_RPCS.",
                        config::rpc_host_label(url)
                    ));
                }
            }
            extras.push(url.to_string());
            let joined = extras.join(",");
            update_env_key("BROADCAST_RPCS", &joined)?;
            Ok(format!(
                "saved\nBROADCAST_RPCS += host {} (n={}). Process env reloaded for this telegram session; restart other binaries to pick up .env.",
                config::rpc_host_label(url),
                extras.len()
            ))
        }
        "/rpc_clear_extra" => {
            update_env_key("BROADCAST_RPCS", "")?;
            Ok("saved\nBROADCAST_RPCS cleared. Process env reloaded for this telegram session; restart other binaries to pick up .env.".into())
        }
        "/rank" => {
            let summary = config::rank_rpc_report().await?;
            Ok(summary)
        }
        "/snipe_public" => {
            // /snipe_public <nft> <qty> <at> [early_ms] [dry]
            if parts.len() < 4 {
                return Ok("usage: /snipe_public <nft> <qty> <at_unix> [early_ms] [dry]".into());
            }
            let nft = parts[1].to_string();
            let qty: u64 = parts[2].parse().wrap_err("qty")?;
            let at: i64 = parts[3].parse().wrap_err("at")?;
            let early_ms: i64 = parts.get(4).and_then(|s| s.parse().ok()).unwrap_or(50);
            let dry = parts.get(5).map(|s| *s == "dry" || *s == "1" || *s == "true").unwrap_or(false);
            crate::outln!("telegram snipe_public nft={nft} qty={qty} at={at} early_ms={early_ms} dry={dry}");
            ops::run_public_snipe(&nft, qty, at, early_ms, dry).await?;
            Ok(format!(
                "snipe_public done dry={dry} nft={nft} qty={qty} at={at} early_ms={early_ms}"
            ))
        }
        "/snipe_wl" => {
            // /snipe_wl <slug> <qty> <at> [early_ms] [dry]
            if parts.len() < 4 {
                return Ok("usage: /snipe_wl <slug> <qty> <at_unix> [early_ms] [dry]".into());
            }
            let slug = parts[1].to_string();
            let qty: u64 = parts[2].parse().wrap_err("qty")?;
            let at: i64 = parts[3].parse().wrap_err("at")?;
            let early_ms: i64 = parts.get(4).and_then(|s| s.parse().ok()).unwrap_or(50);
            let dry = parts.get(5).map(|s| *s == "dry" || *s == "1" || *s == "true").unwrap_or(false);
            crate::outln!("telegram snipe_wl slug={slug} qty={qty} at={at} early_ms={early_ms} dry={dry}");
            ops::run_api_snipe(&slug, qty, at, early_ms, dry).await?;
            Ok(format!(
                "snipe_wl done dry={dry} slug={slug} qty={qty} at={at} early_ms={early_ms}"
            ))
        }
        _ => Ok(format!("unknown command: {cmd}\n{}", help_text())),
    }
}

fn help_text() -> String {
    r#"Commands (authorized chat only):
/doctor — RPC + wallet + OpenSea key presence
/status — wallet address, chain, rpcs, armed files
/rpc — list RPC hosts (redacted; index + host)
/rpc_set <url> — set RPC_URL in .env (saved; no secret echo)
/rpc_add <url> — append to BROADCAST_RPCS
/rpc_clear_extra — clear BROADCAST_RPCS
/rank — rank RPCs by p50 latency (hosts redacted)
/snipe_public <nft> <qty> <at> [early_ms] [dry]
/snipe_wl <slug> <qty> <at> [early_ms] [dry]
/panel_hint — local UI URL
/help

at = unix seconds or ms. Append 'dry' for dry-run (no broadcast).
Private keys / full RPC URLs are never logged or sent."#
        .into()
}

fn status_text() -> String {
    match config::AppConfig::from_env() {
        Ok(cfg) => {
            let armed = std::path::Path::new("armed.json").exists();
            let armed_api = std::path::Path::new("armed-api.json").exists();
            let os = if cfg.opensea_api_key.is_some() {
                "present"
            } else {
                "missing"
            };
            format!(
                "wallet={}\nchain_id={}\nrpcs={}\nopensea_api_key={}\narmed.json={armed}\narmed-api.json={armed_api}\n(never shows WALLET_KEY)",
                cfg.wallet.address(),
                cfg.chain_id,
                cfg.rpc_urls.len(),
                os
            )
        }
        Err(e) => format!("config error: {e}"),
    }
}

async fn capture_doctor() -> Result<String> {
    // Re-run doctor logic; doctor prints via outln — also return a compact summary.
    config::doctor().await?;
    Ok(status_text() + "\ndoctor: OK (see process logs for RPC latency)")
}
