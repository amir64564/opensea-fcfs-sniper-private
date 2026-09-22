// Telegram long-poll control bot (reqwest).
// Never logs WALLET_KEY / private keys / full OpenSea API keys / bot password.
// Access: TELEGRAM_BOT_PASSWORD required; optional TELEGRAM_CHAT_ID allowlist.
// Snipe Setup: select wallet(s) → paste NEW API key per wallet → optional API name
// → Arm → Mint → auto-wipe session API key material.

use crate::config;
use crate::errclass;
use crate::ops;
use crate::session::{self, Phase, SnipeSession};
use crate::task::TaskGate;
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
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub async fn run() -> Result<()> {
    let _ = config::validate_startup();
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
    let gate = TaskGate::new();
    let (job_tx, mut job_rx) = mpsc::unbounded_channel::<JobDone>();
    let mut live: Option<LiveJob> = None;
    let mut shutting_down = false;

    loop {
        if shutting_down {
            if let Some(job) = live.take() {
                crate::task::request_global_cancel();
                job.gate.request_cancel();
                job.handle.abort();
                sess.cleanup("CANCELLED");
                gate.reset_idle();
            }
            crate::outln!("telegram graceful shutdown");
            break;
        }

        // Drain completed background snipes without blocking Telegram.
        while let Ok(done) = job_rx.try_recv() {
            live = None;
            gate.reset_idle();
            crate::task::clear_global_cancel();
            let cleanup_reason = if done.ok { "SUCCESS" } else { "FAILED" };
            sess.cleanup(cleanup_reason);
            let kb = if unlocked.contains(&done.chat_id) {
                keyboard_for(&sess)
            } else {
                locked_keyboard()
            };
            let safe = redact_secrets(&done.message, &sess, &password);
            if let Err(e) = send_kb(&client, &api, &done.chat_id, &safe, kb).await {
                crate::outln!("telegram send_err={e}");
            }
        }

        let poll_timeout = if live.is_some() { 5 } else { 50 };
        let url = format!("{api}/getUpdates?timeout={poll_timeout}&offset={offset}");

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                crate::outln!("telegram received SIGINT/SIGTERM — shutting down");
                shutting_down = true;
                continue;
            }
            resp = client.get(&url).send() => {
                let resp = match resp {
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

                    if let Some(cb) = u.get("callback_query") {
                        let callback_id = cb.get("id").and_then(Value::as_str).unwrap_or("");
                        let data = cb.get("data").and_then(Value::as_str).unwrap_or("").trim();
                        let message_id = cb
                            .get("message")
                            .and_then(|m| m.get("message_id"))
                            .map(|v| v.to_string().trim_matches('"').to_string())
                            .unwrap_or_default();
                        let chat_id = cb
                            .get("message")
                            .and_then(|m| m.get("chat"))
                            .and_then(|c| c.get("id"))
                            .map(|v| v.to_string().trim_matches('"').to_string())
                            .unwrap_or_default();

                        if !callback_id.is_empty() {
                            let _ = client
                                .post(format!("{api}/answerCallbackQuery"))
                                .json(&json!({"callback_query_id": callback_id}))
                                .send()
                                .await;
                        }
                        if chat_id.is_empty() || data.is_empty() {
                            continue;
                        }
                        if let Some(ref allow) = allow_chat {
                            if &chat_id != allow {
                                continue;
                            }
                        }

                        let reply = match gate_and_handle(
                            data,
                            &chat_id,
                            &password,
                            &mut unlocked,
                            &unlock_path,
                            &mut sess,
                            &gate,
                            &job_tx,
                            &mut live,
                        )
                        .await
                        {
                            Ok(s) => s,
                            Err(e) => errclass::telegram_error(&e),
                        };
                        let safe = redact_secrets(&reply, &sess, &password);
                        let kb = if unlocked.contains(&chat_id) {
                            keyboard_for(&sess)
                        } else {
                            locked_keyboard()
                        };
                        if let Err(e) = edit_kb(&client, &api, &chat_id, &message_id, &safe, kb.clone()).await {
                            if let Err(send_err) = send_kb(&client, &api, &chat_id, &safe, kb).await {
                                crate::outln!("telegram callback edit_err={e}; send_err={send_err}");
                            }
                        }
                        continue;
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
                        &gate,
                        &job_tx,
                        &mut live,
                    )
                    .await
                    {
                        Ok(s) => s,
                        Err(e) => errclass::telegram_error(&e),
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
    }
    Ok(())
}

struct LiveJob {
    handle: JoinHandle<()>,
    gate: TaskGate,
    chat_id: String,
}

struct JobDone {
    chat_id: String,
    ok: bool,
    message: String,
}

fn main_keyboard() -> Value {
    telegram_tools::main_menu()
}

fn keyboard_for(sess: &SnipeSession) -> Value {
    ui_keyboard(sess)
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
            && bytes[i + 2..i + 66].iter().all(|c| c.is_ascii_hexdigit())
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
