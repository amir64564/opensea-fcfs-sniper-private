//! Telegram long-poll control bot (reqwest).
//! Never logs WALLET_KEY / private keys / full OpenSea API keys.
//! Snipe Setup: select wallet(s) → paste NEW API key per wallet → optional API name
//! → Arm → Mint → auto-wipe session API key material.

use crate::config;
use crate::ops;
use crate::session::{self, Phase, SnipeSession};
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

    let mut sess = SnipeSession::new();

    let _ = send_kb(
        &client,
        &api,
        &allow_chat,
        "sniper telegram online. Snipe Setup → select wallet → paste NEW OpenSea API key → Arm.\nPrivate keys / full API keys are never logged.",
        main_keyboard(),
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
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let reply = match handle_message(&text, &mut sess).await {
                Ok(s) => s,
                Err(e) => format!("error: {e:#}"),
            };
            let safe = redact_secrets(&reply, &sess);
            let kb = keyboard_for(&sess);
            if let Err(e) = send_kb(&client, &api, &allow_chat, &safe, kb).await {
                crate::outln!("telegram send_err={e}");
            }
        }
    }
}

fn main_keyboard() -> Value {
    json!({
        "keyboard": [
            [{"text": "Snipe Setup"}, {"text": "Arm"}, {"text": "Cancel Session"}],
            [{"text": "/status"}, {"text": "/help"}, {"text": "/wallets"}]
        ],
        "resize_keyboard": true,
        "is_persistent": true
    })
}

fn keyboard_for(_sess: &SnipeSession) -> Value {
    main_keyboard()
}

fn redact_secrets(s: &str, sess: &SnipeSession) -> String {
    let mut cleaned = s.replace("WALLET_KEY", "[redacted]");
    cleaned = cleaned.replace("OPENSEA_API_KEY", "[redacted]");

    // Scrub known session API keys (never echo full key).
    if let Ok(wallets) = session::load_available_wallets() {
        if let Ok(pairs) = sess.wallet_key_pairs(&wallets) {
            for (_w, key) in pairs {
                if key.len() >= 8 {
                    cleaned = cleaned.replace(&key, &session::mask_api_key(&key));
                }
            }
        }
    }
    if let Ok(env_key) = env::var("OPENSEA_API_KEY") {
        if env_key.len() >= 8 && cleaned.contains(&env_key) {
            cleaned = cleaned.replace(&env_key, &session::mask_api_key(&env_key));
        }
    }

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

async fn send_kb(client: &Client, api: &str, chat_id: &str, text: &str, keyboard: Value) -> Result<()> {
    let chunk = if text.len() > 4000 {
        format!("{}…", &text[..4000])
    } else {
        text.to_string()
    };
    let body = json!({
        "chat_id": chat_id,
        "text": chunk,
        "disable_web_page_preview": true,
        "reply_markup": keyboard,
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

fn norm_cmd(s: &str) -> String {
    s.split('@').next().unwrap_or(s).to_lowercase()
}

async fn handle_message(text: &str, sess: &mut SnipeSession) -> Result<String> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.is_empty() {
        return Ok("empty".into());
    }
    let raw0 = parts[0];
    let cmd = norm_cmd(raw0);

    // --- Phase-aware free-text handling (before command match) ---
    // Wallet name after import
    if sess.is_awaiting_wallet_name() {
        if matches!(cmd.as_str(), "/cancel" | "cancel session" | "/cancel_session") {
            sess.cleanup("CANCELLED");
            return Ok("Session cancelled. OpenSea API keys wiped.".into());
        }
        if cmd == "/skip" || cmd == "skip" {
            if let Phase::AwaitWalletName { address } = sess.phase.clone() {
                let _ = session::set_wallet_display_name(address, &format!("w-{}", session::short_addr(&address)));
            }
            sess.phase = Phase::Idle;
            return Ok("Wallet name skipped (default used). Use /rename_wallet later.".into());
        }
        if !cmd.starts_with('/') && cmd != "snipe setup" && cmd != "arm" && cmd != "cancel session" {
            if let Phase::AwaitWalletName { address } = sess.phase.clone() {
                let msg = session::set_wallet_display_name(address, text)?;
                sess.phase = Phase::Idle;
                return Ok(msg);
            }
        }
    }

    // API name after key paste
    if sess.is_awaiting_api_name() {
        if matches!(cmd.as_str(), "/cancel" | "cancel session" | "/cancel_session") {
            sess.cleanup("CANCELLED");
            return Ok("Session cancelled. OpenSea API keys wiped.".into());
        }
        let wallets = session::load_available_wallets()?;
        if cmd == "/skip" || cmd == "skip" {
            return sess.set_api_name(&wallets, "", true);
        }
        if !cmd.starts_with('/') && cmd != "snipe setup" && cmd != "arm" && cmd != "cancel session" {
            return sess.set_api_name(&wallets, text, false);
        }
    }

    // API key paste
    if sess.is_awaiting_api_key() {
        if matches!(cmd.as_str(), "/cancel" | "cancel session" | "/cancel_session") {
            sess.cleanup("CANCELLED");
            return Ok("Session cancelled. OpenSea API keys wiped.".into());
        }
        // Treat non-command text as the API key (never log it).
        if !cmd.starts_with('/') && cmd != "snipe setup" && cmd != "arm" && cmd != "cancel session" {
            crate::outln!("telegram: received session OpenSea API key paste (not logged)");
            let wallets = session::load_available_wallets()?;
            return sess.attach_api_key(&wallets, text);
        }
    }

    // Wallet pick
    if sess.is_picking_wallets() {
        if matches!(cmd.as_str(), "/cancel" | "cancel session" | "/cancel_session") {
            sess.cleanup("CANCELLED");
            return Ok("Session cancelled.".into());
        }
        if !cmd.starts_with('/') && cmd != "snipe setup" && cmd != "arm" && cmd != "cancel session" {
            let wallets = session::load_available_wallets()?;
            let idxs = session::parse_wallet_selection(text, wallets.len())?;
            return sess.select_wallets(&wallets, &idxs);
        }
    }

    // Button labels (exact / case-insensitive)
    let button = text.to_lowercase();
    if button == "snipe setup" || cmd == "/snipe_setup" || cmd == "snipe_setup" {
        return start_snipe_setup(sess);
    }
    if button == "cancel session" || cmd == "/cancel_session" || cmd == "/cancel" {
        sess.cleanup("CANCELLED");
        return Ok("Session cancelled. Temporary OpenSea API keys wiped. Wallets untouched.".into());
    }
    if button == "arm" || cmd == "/arm" {
        return handle_arm_button(sess);
    }

    match cmd.as_str() {
        "/help" | "help" => Ok(help_text()),
        "/doctor" => {
            config::doctor().await?;
            Ok(status_text(sess) + "\ndoctor: OK (see process logs for RPC latency)")
        }
        "/status" => Ok(status_text(sess)),
        "/wallets" => {
            let wallets = session::load_available_wallets()?;
            Ok(session::format_wallet_list(&wallets))
        }
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
        "/import_wallet" => {
            if parts.len() < 2 {
                return Ok("usage: /import_wallet <private_key>\n(key is never echoed back)".into());
            }
            let pk = parts[1];
            // Do not log pk
            crate::outln!("telegram: import_wallet requested (key not logged)");
            let addr = session::import_wallet(pk)?;
            sess.begin_await_wallet_name(addr);
            Ok(format!(
                "Wallet imported: {}\n\nSet your wallet name:",
                session::short_addr(&addr)
            ))
        }
        "/rename_wallet" => {
            if parts.len() < 3 {
                return Ok("usage: /rename_wallet <index|label|address> <name>".into());
            }
            let target = parts[1];
            let name = parts[2..].join(" ");
            let wallets = session::load_available_wallets()?;
            session::rename_wallet(&wallets, target, &name)
        }
        "/rename_api" => {
            if parts.len() < 3 {
                return Ok("usage: /rename_api <selected_index|label|address> <name>\n(session label only; secret still wiped at session end)".into());
            }
            let target = parts[1];
            let name = parts[2..].join(" ");
            let wallets = session::load_available_wallets()?;
            sess.rename_api(&wallets, target, &name)
        }
        "/skip" => {
            if sess.is_awaiting_api_name() {
                let wallets = session::load_available_wallets()?;
                return sess.set_api_name(&wallets, "", true);
            }
            Ok("nothing to skip".into())
        }
        "/session" => Ok(session_status(sess)),
        "/snipe_public" => run_snipe_public(parts, sess).await,
        "/snipe_wl" => run_snipe_wl(parts, sess).await,
        // Shorthand mint params when ReadyToArm: wl <slug> ... / public <nft> ...
        "wl" if sess.is_ready() || sess.is_running() => {
            let mut p = vec!["/snipe_wl"];
            p.extend_from_slice(&parts[1..]);
            run_snipe_wl(p, sess).await
        }
        "public" if sess.is_ready() || sess.is_running() => {
            let mut p = vec!["/snipe_public"];
            p.extend_from_slice(&parts[1..]);
            run_snipe_public(p, sess).await
        }
        _ => Ok(format!("unknown command: {cmd}\n{}", help_text())),
    }
}

fn start_snipe_setup(sess: &mut SnipeSession) -> Result<String> {
    if sess.is_running() {
        eyre::bail!("session is Running — Cancel Session first or wait for mint to finish");
    }
    // Starting a new setup wipes any prior session API keys.
    sess.start_setup();
    let wallets = session::load_available_wallets()?;
    Ok(format!(
        "Snipe Setup\n\n{}\n\n(No saved API-key list — you will paste a NEW key per wallet for this session only.)",
        session::format_wallet_list(&wallets)
    ))
}

fn handle_arm_button(sess: &mut SnipeSession) -> Result<String> {
    if !sess.is_ready() {
        if sess.is_idle() {
            return Ok("No live session. Press Snipe Setup first (select wallet → paste NEW API key).".into());
        }
        return Ok(format!(
            "Session not ready (phase={:?}). Finish API key paste / name, or Cancel Session.",
            sess.phase
        ));
    }
    let wallets = session::load_available_wallets()?;
    Ok(format!(
        "Armed (session keys loaded).\nMap:\n{}\n\nSend mint params:\n  wl <slug> <qty> <at> [early_ms] [dry]\n  public <nft> <qty> <at> [early_ms] [dry]\nOr /snipe_wl / /snipe_public\n\nAfter SUCCESS / FAILED / TIMEOUT / CANCELLED, session API keys are wiped automatically.",
        sess.map_summary(&wallets)
    ))
}

fn session_status(sess: &SnipeSession) -> String {
    let wallets = session::load_available_wallets().unwrap_or_default();
    format!(
        "phase={:?}\nselected={}\nsession_api_keys={}\n{}",
        sess.phase,
        sess.selected.len(),
        sess.selected.iter().filter(|&&i| {
            wallets.get(i).map(|w| sess.key_for(&w.address).is_some()).unwrap_or(false)
        }).count(),
        if wallets.is_empty() {
            String::new()
        } else if sess.selected.is_empty() {
            String::new()
        } else {
            format!("map:\n{}", sess.map_summary(&wallets))
        }
    )
}

async fn run_snipe_wl(parts: Vec<&str>, sess: &mut SnipeSession) -> Result<String> {
    if parts.len() < 4 {
        return Ok("usage: /snipe_wl <slug> <qty> <at_unix> [early_ms] [dry]".into());
    }
    let slug = parts[1].to_string();
    let qty: u64 = parts[2].parse().wrap_err("qty")?;
    let at: i64 = parts[3].parse().wrap_err("at")?;
    let early_ms: i64 = parts.get(4).and_then(|s| s.parse().ok()).unwrap_or(50);
    let dry = parts
        .get(5)
        .map(|s| *s == "dry" || *s == "1" || *s == "true")
        .unwrap_or(false);

    let wallets = session::load_available_wallets()?;
    let use_session = sess.can_mint();

    if use_session {
        sess.mark_running();
        let pairs = match sess.wallet_key_pairs(&wallets) {
            Ok(p) => p,
            Err(e) => {
                sess.cleanup("FAILED");
                return Err(e);
            }
        };
        crate::outln!(
            "telegram snipe_wl session wallets={} slug={slug} qty={qty} at={at} early_ms={early_ms} dry={dry}",
            pairs.len()
        );
        let mut reports = Vec::new();
        let mut any_fail = false;
        let mut timed_out = false;
        for (w, api_key) in &pairs {
            let label = session::display_wallet(w);
            crate::outln!("telegram snipe_wl wallet={label} (api key masked in chat)");
            match ops::run_api_snipe_with(
                &slug,
                qty,
                at,
                early_ms,
                dry,
                Some(&w.private_key),
                Some(api_key),
            )
            .await
            {
                Ok(()) => reports.push(format!("OK {label}")),
                Err(e) => {
                    let es = format!("{e:#}");
                    if es.to_lowercase().contains("timed out") || es.to_lowercase().contains("timeout") {
                        timed_out = true;
                    }
                    any_fail = true;
                    reports.push(format!("FAIL {label}: {es}"));
                }
            }
        }
        let reason = if timed_out {
            "TIMEOUT"
        } else if any_fail {
            "FAILED"
        } else {
            "SUCCESS"
        };
        sess.cleanup(reason);
        Ok(format!(
            "snipe_wl {reason}\nslug={slug} qty={qty} at={at} early_ms={early_ms} dry={dry}\n{}\n(session OpenSea API keys wiped)",
            reports.join("\n")
        ))
    } else {
        // Fallback: env OPENSEA_API_KEY + WALLET_KEY (no session)
        crate::outln!("telegram snipe_wl (env keys) slug={slug} qty={qty} at={at} early_ms={early_ms} dry={dry}");
        match ops::run_api_snipe(&slug, qty, at, early_ms, dry).await {
            Ok(()) => Ok(format!(
                "snipe_wl done dry={dry} slug={slug} qty={qty} at={at} early_ms={early_ms}"
            )),
            Err(e) => Err(e),
        }
    }
}

async fn run_snipe_public(parts: Vec<&str>, sess: &mut SnipeSession) -> Result<String> {
    if parts.len() < 4 {
        return Ok("usage: /snipe_public <nft> <qty> <at_unix> [early_ms] [dry]".into());
    }
    let nft = parts[1].to_string();
    let qty: u64 = parts[2].parse().wrap_err("qty")?;
    let at: i64 = parts[3].parse().wrap_err("at")?;
    let early_ms: i64 = parts.get(4).and_then(|s| s.parse().ok()).unwrap_or(50);
    let dry = parts
        .get(5)
        .map(|s| *s == "dry" || *s == "1" || *s == "true")
        .unwrap_or(false);

    let wallets = session::load_available_wallets()?;
    let use_session = sess.has_selected() && (sess.is_ready() || sess.can_mint() || matches!(sess.phase, Phase::ReadyToArm | Phase::Running));

    // Public path doesn't need OpenSea keys; use selected session wallets when present.
    if use_session {
        sess.mark_running();
        let mut reports = Vec::new();
        let mut any_fail = false;
        for &idx in &sess.selected {
            let w = match wallets.get(idx) {
                Some(w) => w,
                None => continue,
            };
            let label = session::display_wallet(w);
            match ops::run_public_snipe_with(&nft, qty, at, early_ms, dry, Some(&w.private_key)).await
            {
                Ok(()) => reports.push(format!("OK {label}")),
                Err(e) => {
                    any_fail = true;
                    reports.push(format!("FAIL {label}: {e:#}"));
                }
            }
        }
        let reason = if any_fail { "FAILED" } else { "SUCCESS" };
        sess.cleanup(reason);
        Ok(format!(
            "snipe_public {reason}\nnft={nft} qty={qty} at={at} early_ms={early_ms} dry={dry}\n{}\n(session OpenSea API keys wiped)",
            reports.join("\n")
        ))
    } else {
        crate::outln!("telegram snipe_public nft={nft} qty={qty} at={at} early_ms={early_ms} dry={dry}");
        ops::run_public_snipe(&nft, qty, at, early_ms, dry).await?;
        Ok(format!(
            "snipe_public done dry={dry} nft={nft} qty={qty} at={at} early_ms={early_ms}"
        ))
    }
}

fn help_text() -> String {
    r#"Commands (authorized chat only):
Snipe Setup — select wallet(s) → paste NEW OpenSea API key → optional API name → Arm
Arm — show mint params once session keys are attached
Cancel Session — wipe temporary OpenSea API keys

/import_wallet <pk> — import wallet, then "Set your wallet name"
/rename_wallet <index|label|addr> <name>
/rename_api <selected_index|label|addr> <name>  (session label only)
/wallets /session /status /doctor /help
/rpc /rpc_set /rpc_add /rpc_clear_extra /rank
/snipe_public <nft> <qty> <at> [early_ms] [dry]
/snipe_wl <slug> <qty> <at> [early_ms] [dry]

Session API keys are wiped after SUCCESS / FAILED / TIMEOUT / CANCELLED.
Wallet private keys + display names persist. No permanent API_1 vault.
Private keys / full API keys are never logged or sent."#
        .into()
}

fn status_text(sess: &SnipeSession) -> String {
    let base = match config::AppConfig::from_env() {
        Ok(cfg) => {
            let armed = Path::new("armed.json").exists();
            let armed_api = Path::new("armed-api.json").exists();
            let os = if cfg.opensea_api_key.is_some() {
                "env-present"
            } else {
                "env-missing"
            };
            format!(
                "wallet={}\nchain_id={}\nrpcs={}\nopensea_api_key={}\narmed.json={armed}\narmed-api.json={armed_api}",
                cfg.wallet.address(),
                cfg.chain_id,
                cfg.rpc_urls.len(),
                os
            )
        }
        Err(e) => format!("config error: {e}"),
    };
    format!("{base}\n{}", session_status(sess))
}
