//! Telegram long-poll control bot (reqwest).
//! Never logs WALLET_KEY / private keys / full OpenSea API keys / bot password.
//! Access: TELEGRAM_BOT_PASSWORD required; optional TELEGRAM_CHAT_ID allowlist.
//! Snipe Setup: select wallet(s) → paste NEW API key per wallet → optional API name
//! → Arm → Mint → auto-wipe session API key material.

use crate::config;
use crate::ops;
use crate::session::{self, Phase, SnipeSession};
use eyre::{Result, WrapErr};
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub async fn run() -> Result<()> {
    let token = env::var("TELEGRAM_BOT_TOKEN")
        .wrap_err("TELEGRAM_BOT_TOKEN missing — create a bot via @BotFather and set env")?;
    let password = env::var("TELEGRAM_BOT_PASSWORD")
        .wrap_err("TELEGRAM_BOT_PASSWORD missing — required to unlock telegram control")?
        .trim()
        .to_string();
    if token.is_empty() || password.is_empty() {
        eyre::bail!("TELEGRAM_BOT_TOKEN and TELEGRAM_BOT_PASSWORD must be non-empty");
    }
    let allow_chat = env::var("TELEGRAM_CHAT_ID")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

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
    let unlock_path = unlocked_path();
    let mut unlocked = load_unlocked(&unlock_path);
    match &allow_chat {
        Some(id) => crate::outln!(
            "telegram bot online @{uname} allow_chat_id={id} password=set unlocked={}",
            unlocked.len()
        ),
        None => crate::outln!(
            "telegram bot online @{uname} allow_chat_id=(any after password) password=set unlocked={}",
            unlocked.len()
        ),
    }

    let mut sess = SnipeSession::new();
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
            if chat_id.is_empty() {
                continue;
            }
            if let Some(ref allow) = allow_chat {
                if &chat_id != allow {
                    crate::outln!("telegram ignore non-allowlisted chat_id={chat_id}");
                    continue;
                }
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
            let reply = match gate_and_handle(
                &text,
                &chat_id,
                &password,
                &mut unlocked,
                &unlock_path,
                &mut sess,
            )
            .await
            {
                Ok(s) => s,
                Err(e) => format!("error: {e:#}"),
            };
            let safe = redact_secrets(&reply, &sess, &password);
            let kb = if unlocked.contains(&chat_id) {
                keyboard_for(&sess)
            } else {
                locked_keyboard()
            };
            if let Err(e) = send_kb(&client, &api, &chat_id, &safe, kb).await {
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

fn redact_secrets(s: &str, sess: &SnipeSession, password: &str) -> String {
    let mut cleaned = s.replace("WALLET_KEY", "[redacted]");
    cleaned = cleaned.replace("OPENSEA_API_KEY", "[redacted]");
    if password.len() >= 4 && cleaned.contains(password) {
        cleaned = cleaned.replace(password, "[redacted_password]");
    }

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


const UNLOCKED_FILE: &str = "telegram_unlocked.json";

fn unlocked_path() -> PathBuf {
    PathBuf::from(UNLOCKED_FILE)
}

fn load_unlocked(path: &Path) -> HashSet<String> {
    let Ok(raw) = fs::read_to_string(path) else {
        return HashSet::new();
    };
    match serde_json::from_str::<Vec<String>>(&raw) {
        Ok(v) => v.into_iter().filter(|s| !s.is_empty()).collect(),
        Err(_) => HashSet::new(),
    }
}

fn save_unlocked(path: &Path, set: &HashSet<String>) {
    let mut list: Vec<String> = set.iter().cloned().collect();
    list.sort();
    let Ok(bytes) = serde_json::to_vec_pretty(&list) else {
        return;
    };
    let _ = fs::remove_file(path);
    match fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut f) => {
            let _ = f.write_all(&bytes);
            let _ = f.write_all(b"\n");
        }
        Err(e) => crate::outln!("WARN: could not persist unlocked chats: {e}"),
    }
}

/// Constant-time equality for password bytes (length mismatch → false, still scans).
fn ct_eq(a: &str, b: &str) -> bool {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    let len = ab.len().max(bb.len());
    let mut diff: u8 = if ab.len() == bb.len() { 0 } else { 1 };
    for i in 0..len {
        let x = *ab.get(i).unwrap_or(&0);
        let y = *bb.get(i).unwrap_or(&0);
        diff |= x ^ y;
    }
    diff == 0
}

fn locked_keyboard() -> Value {
    json!({
        "keyboard": [
            [{"text": "/start"}, {"text": "/password"}]
        ],
        "resize_keyboard": true,
        "is_persistent": true
    })
}

async fn gate_and_handle(
    text: &str,
    chat_id: &str,
    password: &str,
    unlocked: &mut HashSet<String>,
    unlock_path: &Path,
    sess: &mut SnipeSession,
) -> Result<String> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    let cmd = parts
        .first()
        .map(|s| norm_cmd(s))
        .unwrap_or_default();

    if cmd == "/start" || cmd == "start" {
        if unlocked.contains(chat_id) {
            return Ok(
                "unlocked. Snipe Setup → select wallet → paste NEW OpenSea API key → Arm.\nPrivate keys / full API keys / password are never logged."
                    .into(),
            );
        }
        return Ok(
            "locked. Unlock with `/password <code>` (then Snipe Setup / Arm).\nPrivate keys / password are never logged."
                .into(),
        );
    }

    if cmd == "/password" || cmd == "password" {
        if parts.len() < 2 {
            return Ok("usage: /password <code>".into());
        }
        let provided = parts[1..].join(" ");
        // Never log provided or expected password.
        if ct_eq(provided.trim(), password) {
            unlocked.insert(chat_id.to_string());
            save_unlocked(unlock_path, unlocked);
            crate::outln!("telegram unlocked chat_id={chat_id}");
            return Ok("unlocked. You can use Snipe Setup / Arm / commands.".into());
        }
        crate::outln!("telegram bad password attempt chat_id={chat_id}");
        return Ok("wrong password".into());
    }

    if cmd == "/lock" || cmd == "/logout" || cmd == "lock" || cmd == "logout" {
        unlocked.remove(chat_id);
        save_unlocked(unlock_path, unlocked);
        sess.cleanup("CANCELLED");
        crate::outln!("telegram locked chat_id={chat_id}");
        return Ok("locked for this chat. Use /password <code> to unlock again.".into());
    }

    if !unlocked.contains(chat_id) {
        return Ok("locked — /password first".into());
    }

    handle_message(text, sess).await
}
