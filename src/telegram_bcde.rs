
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

async fn send_kb(
    client: &Client,
    api: &str,
    chat_id: &str,
    text: &str,
    keyboard: Value,
) -> Result<()> {
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
    gate: &TaskGate,
    job_tx: &mpsc::UnboundedSender<JobDone>,
    live: &mut Option<LiveJob>,
) -> Result<String> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    let cmd = parts.first().map(|s| norm_cmd(s)).unwrap_or_default();

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

    // Cancel Session / lock also abort any live background snipe.
    if text.to_lowercase() == "cancel session"
        || matches!(
            norm_cmd(text.split_whitespace().next().unwrap_or("")).as_str(),
            "/cancel" | "/cancel_session"
        )
    {
        crate::task::request_global_cancel();
        gate.request_cancel();
        if let Some(job) = live.take() {
            job.handle.abort();
        }
        gate.reset_idle();
        crate::task::clear_global_cancel();
    }
    handle_message(text, sess, chat_id, gate, job_tx, live).await
}

async fn handle_message(
    text: &str,
    sess: &mut SnipeSession,
    chat_id: &str,
    gate: &TaskGate,
    job_tx: &mpsc::UnboundedSender<JobDone>,
    live: &mut Option<LiveJob>,
) -> Result<String> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.is_empty() {
        return Ok("empty".into());
    }
    let raw0 = parts[0];
    let cmd = norm_cmd(raw0);

    // --- Phase-aware free-text handling (before command match) ---
    // Wallet name after import
    if sess.is_awaiting_wallet_name() {
        if matches!(
            cmd.as_str(),
            "/cancel" | "cancel session" | "/cancel_session"
        ) {
            sess.cleanup("CANCELLED");
            return Ok("Session cancelled. OpenSea API keys wiped.".into());
        }
        if cmd == "/skip" || cmd == "skip" {
            if let Phase::AwaitWalletName { address } = sess.phase.clone() {
                let _ = session::set_wallet_display_name(
                    address,
                    &format!("w-{}", session::short_addr(&address)),
                );
            }
            sess.phase = Phase::Idle;
            return Ok("Wallet name skipped (default used). Use /rename_wallet later.".into());
        }
        if !cmd.starts_with('/') && cmd != "snipe setup" && cmd != "arm" && cmd != "cancel session"
        {
            if let Phase::AwaitWalletName { address } = sess.phase.clone() {
                let msg = session::set_wallet_display_name(address, text)?;
                sess.phase = Phase::Idle;
                return Ok(msg);
            }
        }
    }

    // API name after key paste
    if sess.is_awaiting_api_name() {
        if matches!(
            cmd.as_str(),
            "/cancel" | "cancel session" | "/cancel_session"
        ) {
            sess.cleanup("CANCELLED");
            return Ok("Session cancelled. OpenSea API keys wiped.".into());
        }
        let wallets = session::load_available_wallets()?;
        if cmd == "/skip" || cmd == "skip" {
            return sess.set_api_name(&wallets, "", true);
        }
        if !cmd.starts_with('/') && cmd != "snipe setup" && cmd != "arm" && cmd != "cancel session"
        {
            return sess.set_api_name(&wallets, text, false);
        }
    }

    // API key paste
    if sess.is_awaiting_api_key() {
        if matches!(
            cmd.as_str(),
            "/cancel" | "cancel session" | "/cancel_session"
        ) {
            sess.cleanup("CANCELLED");
            return Ok("Session cancelled. OpenSea API keys wiped.".into());
        }
        // Treat non-command text as the API key (never log it).
        if !cmd.starts_with('/') && cmd != "snipe setup" && cmd != "arm" && cmd != "cancel session"
        {
            crate::outln!("telegram: received session OpenSea API key paste (not logged)");
            let wallets = session::load_available_wallets()?;
            return sess.attach_api_key(&wallets, text);
        }
    }

    // Wallet pick
    if sess.is_picking_wallets() {
        if matches!(
            cmd.as_str(),
            "/cancel" | "cancel session" | "/cancel_session"
        ) {
            sess.cleanup("CANCELLED");
            return Ok("Session cancelled.".into());
        }
        if !cmd.starts_with('/') && cmd != "snipe setup" && cmd != "arm" && cmd != "cancel session"
        {
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
        crate::task::request_global_cancel();
        gate.request_cancel();
        if let Some(job) = live.take() {
            job.handle.abort();
        }
        gate.reset_idle();
        sess.cleanup("CANCELLED");
        crate::task::clear_global_cancel();
        return Ok("Session cancelled. Countdown/fire abort requested. Temporary OpenSea API keys wiped. Wallets untouched.".into());
    }
    if button == "arm" || cmd == "/arm" {
        return handle_arm_button(sess);
    }


    // Screenshot-inspired utility menu. Reply-keyboard buttons are normal text messages in Telegram.
    if button == "🔐 snipe setup" {
        return start_snipe_setup(sess);
    }
    if button == "⚡ arm" {
        return handle_arm_button(sess);
    }
    if button == "❌ cancel session" {
        crate::task::request_global_cancel();
        gate.request_cancel();
        if let Some(job) = live.take() {
            job.handle.abort();
        }
        gate.reset_idle();
        sess.cleanup("CANCELLED");
        crate::task::clear_global_cancel();
        return Ok("Session cancelled. Countdown/fire abort requested. Temporary OpenSea API keys wiped. Wallets untouched.".into());
    }
    if button == "👛 wallets" {
        let wallets = session::load_available_wallets()?;
        return Ok(session::format_wallet_list(&wallets));
    }
    if button == "🌐 rpc" {
        return Ok(list_rpcs_text());
    }

    if matches!(
        button.as_str(),
        "🎨 mint nft" | "🎯 snipe" | "📦 batch mint" | "🎯 batch snipe" |
        "🔧 manual mint" | "🎛️ exec" | "🎯 my snipes" | "📤 send nfts" |
        "📤 batch send" | "🔥 burn nfts" | "🏦 consolidate" | "🔍 eligibility" |
        "💸 disperse eth" | "💸 send eth" | "👛 wallets" | "🌐 rpc" |
        "⚙️ settings"
    ) {
        return Ok(feature_button_text(&button));
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
        "/snipe_public" => run_snipe_public(parts, sess, chat_id, gate, job_tx, live).await,
        "/snipe_wl" => run_snipe_wl(parts, sess, chat_id, gate, job_tx, live).await,
        "/mint" => feature_mint(parts, sess).await,
        "/batch_mint" => feature_batch_mint(parts, sess).await,
        "/batch_snipe" => feature_batch_snipe(parts, sess).await,
        "/manual_mint" => feature_manual_mint(parts, sess).await,
        "/exec" => feature_exec(parts, sess).await,
        "/my_snipes" => Ok(telegram_tools::my_snipes()),
        "/send_nft" => feature_send_nft(parts, sess).await,
        "/batch_send" => feature_batch_send(parts, sess).await,
        "/burn" => feature_burn(parts, sess).await,
        "/consolidate" => feature_consolidate(parts, sess).await,
        "/eligibility" => feature_eligibility(parts, sess).await,
        "/disperse_eth" => feature_disperse_eth(parts, sess).await,
        "/send_eth" => feature_send_eth(parts, sess).await,
        // Shorthand mint params when ReadyToArm: wl <slug> ... / public <nft> ...
        "wl" if sess.is_ready() || sess.is_running() => {
            let mut p = vec!["/snipe_wl"];
            p.extend_from_slice(&parts[1..]);
            run_snipe_wl(p, sess, chat_id, gate, job_tx, live).await
        }
        "public" if sess.is_ready() || sess.is_running() => {
            let mut p = vec!["/snipe_public"];
            p.extend_from_slice(&parts[1..]);
            run_snipe_public(p, sess, chat_id, gate, job_tx, live).await
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
            return Ok(
                "No live session. Press Snipe Setup first (select wallet → paste NEW API key)."
                    .into(),
            );
        }
        return Ok(format!(
            "Session not ready (phase={:?}). Finish API key paste / name, or Cancel Session.",
            sess.phase
        ));
    }
    let wallets = session::load_available_wallets()?;
    Ok(format!(
        "Armed (session keys loaded).\nMap:\n{}\n\nSend mint params (omit time or use auto):\n  wl <slug> <qty> [at|auto] [early_ms] [dry]\n  public <nft> <qty> [at|auto] [early_ms] [dry]\nOr /snipe_wl / /snipe_public\n\nAfter SUCCESS / FAILED / TIMEOUT / CANCELLED, session API keys are wiped automatically.",
        sess.map_summary(&wallets)
    ))
}

fn session_status(sess: &SnipeSession) -> String {
    let wallets = session::load_available_wallets().unwrap_or_default();
    format!(
        "phase={:?}\nselected={}\nsession_api_keys={}\n{}",
        sess.phase,
        sess.selected.len(),
        sess.selected
            .iter()
            .filter(|&&i| {
                wallets
                    .get(i)
                    .map(|w| sess.key_for(&w.address).is_some())
                    .unwrap_or(false)
            })
            .count(),
        if wallets.is_empty() {
            String::new()
        } else if sess.selected.is_empty() {
            String::new()
        } else {
            format!("map:\n{}", sess.map_summary(&wallets))
        }
    )
}

async fn run_snipe_wl(
    parts: Vec<&str>,
    sess: &mut SnipeSession,
    chat_id: &str,
    gate: &TaskGate,
    job_tx: &mpsc::UnboundedSender<JobDone>,
    live: &mut Option<LiveJob>,
) -> Result<String> {
    if parts.len() < 3 {
        return Ok("usage: /snipe_wl <slug> <qty> [at|auto] [early_ms] [dry]\n(omit at or use auto → OpenSea stage startTime)".into());
    }
    if live.is_some() || gate.is_active() {
        return Ok(format!(
            "busy — task already {} (Cancel Session first)",
            gate.state().as_str()
        ));
    }
    let slug = parts[1].to_string();
    let slug_ack = slug.clone();
    let qty: u64 = parts[2].parse().wrap_err("qty")?;
    let (at, early_ms, dry) = parse_snipe_tail(&parts[3..])?;

    let wallets = session::load_available_wallets()?;
    let use_session = sess.can_mint();

    gate.begin_waiting()?;
    crate::task::clear_global_cancel();
    sess.mark_running();

    let chat = chat_id.to_string();
    let tx = job_tx.clone();
    let gate_job = gate.clone();


    if use_session {
        let pairs = match sess.wallet_key_pairs(&wallets) {
            Ok(p) => p
                .into_iter()
                .map(|(w, k)| (session::display_wallet(&w), w.private_key.clone(), k))
                .collect::<Vec<_>>(),
            Err(e) => {
                sess.cleanup("FAILED");
                gate.reset_idle();
                return Err(e);
            }
        };
        let handle = tokio::spawn(async move {
            gate_job.set_armed();
            let mut reports = Vec::new();
            let mut any_fail = false;
            let mut timed_out = false;
            for (label, pk, api_key) in &pairs {
                if crate::task::global_cancelled() {
                    any_fail = true;
                    reports.push(format!("CANCELLED {label}"));
                    break;
                }
                crate::outln!("telegram snipe_wl wallet={label} (api key masked in chat)");
                if !gate_job.try_begin_firing() && gate_job.is_cancelled() {
                    reports.push(format!("CANCELLED {label}"));
                    any_fail = true;
                    break;
                }
                match ops::run_api_snipe_with(
                    &slug,
                    qty,
                    at,
                    early_ms,
                    dry,
                    Some(pk),
                    Some(api_key),
                )
                .await
                {
                    Ok(()) => reports.push(format!("OK {label}")),
                    Err(e) => {
                        let es = errclass::sanitize(&format!("{e:#}"));
                        let kind = errclass::classify(&es).as_str();
                        if kind == "timeout" || kind == "cancelled" {
                            timed_out = kind == "timeout";
                        }
                        any_fail = true;
                        reports.push(format!("FAIL {label} [{kind}]: {es}"));
                    }
                }
            }
            let reason = if crate::task::global_cancelled() {
                gate_job.request_cancel();
                "CANCELLED"
            } else if timed_out {
                gate_job.finish_failed();
                "TIMEOUT"
            } else if any_fail {
                gate_job.finish_failed();
                "FAILED"
            } else {
                gate_job.finish_success();
                "SUCCESS"
            };
            let message = format!(
                "snipe_wl {reason}\nslug={slug} qty={qty} at={at:?} early_ms={early_ms} dry={dry}\n{}\n(session OpenSea API keys wiped)",
                reports.join("\n")
            );
            let _ = tx.send(JobDone {
                chat_id: chat,
                ok: reason == "SUCCESS",
                message,
            });
        });
        *live = Some(LiveJob {
            handle,
            gate: gate.clone(),
            chat_id: chat_id.to_string(),
        });
        Ok(format!(
            "WAITING → ARMED → FIRING\nmode=wl slug={slug_ack} qty={qty} at={at:?} early_ms={early_ms} dry={dry}\nTelegram stays responsive — Cancel Session aborts countdown.\nResult will be sent when finished."
        ))
    } else {
        let handle = tokio::spawn(async move {
            gate_job.set_armed();
            let _ = gate_job.try_begin_firing();
            let message = match ops::run_api_snipe(&slug, qty, at, early_ms, dry).await {
                Ok(()) => {
                    gate_job.finish_success();
                    format!("snipe_wl SUCCESS dry={dry} slug={slug} qty={qty} at={at:?} early_ms={early_ms}")
                }
                Err(e) => {
                    gate_job.finish_failed();
                    errclass::telegram_error(&e)
                }
            };
            let ok = message.contains("SUCCESS");
            let _ = tx.send(JobDone {
                chat_id: chat,
                ok,
                message,
            });
        });
        *live = Some(LiveJob {
            handle,
            gate: gate.clone(),
            chat_id: chat_id.to_string(),
        });
        Ok(format!(
            "WAITING (env keys)\nmode=wl slug={slug_ack} qty={qty} at={at:?} early_ms={early_ms} dry={dry}\nResult will be sent when finished."
        ))
    }
}

async fn run_snipe_public(
    parts: Vec<&str>,
    sess: &mut SnipeSession,
    chat_id: &str,
    gate: &TaskGate,
    job_tx: &mpsc::UnboundedSender<JobDone>,
    live: &mut Option<LiveJob>,
) -> Result<String> {
    if parts.len() < 3 {
        return Ok("usage: /snipe_public <nft> <qty> [at|auto] [early_ms] [dry]\n(omit at or use auto → on-chain getPublicDrop startTime)".into());
    }
    if live.is_some() || gate.is_active() {
        return Ok(format!(
            "busy — task already {} (Cancel Session first)",
            gate.state().as_str()
        ));
    }
    let nft = parts[1].to_string();
    let nft_ack = nft.clone();
    let qty: u64 = parts[2].parse().wrap_err("qty")?;
    let (at, early_ms, dry) = parse_snipe_tail(&parts[3..])?;

    let wallets = session::load_available_wallets()?;
    let use_session = sess.has_selected()
        && (sess.is_ready()
            || sess.can_mint()
            || matches!(sess.phase, Phase::ReadyToArm | Phase::Running));

    gate.begin_waiting()?;
    crate::task::clear_global_cancel();
    if use_session {
        sess.mark_running();
    }
    let chat = chat_id.to_string();
    let tx = job_tx.clone();
    let gate_job = gate.clone();

    if use_session {
        let jobs: Vec<(String, String)> = sess
            .selected
            .iter()
            .filter_map(|&idx| {
                wallets
                    .get(idx)
                    .map(|w| (session::display_wallet(w), w.private_key.clone()))
            })
            .collect();
        let handle = tokio::spawn(async move {
            gate_job.set_armed();
            let mut reports = Vec::new();
            let mut any_fail = false;
            for (label, pk) in &jobs {
                if crate::task::global_cancelled() {
                    reports.push(format!("CANCELLED {label}"));
                    any_fail = true;
                    break;
                }
                let _ = gate_job.try_begin_firing();
                match ops::run_public_snipe_with(&nft, qty, at, early_ms, dry, Some(pk)).await {
                    Ok(()) => reports.push(format!("OK {label}")),
                    Err(e) => {
                        any_fail = true;
                        let es = errclass::sanitize(&format!("{e:#}"));
                        let kind = errclass::classify(&es).as_str();
                        reports.push(format!("FAIL {label} [{kind}]: {es}"));
                    }
                }
            }
            let reason = if crate::task::global_cancelled() {
                gate_job.request_cancel();
                "CANCELLED"
            } else if any_fail {
                gate_job.finish_failed();
                "FAILED"
            } else {
                gate_job.finish_success();
                "SUCCESS"
            };
            let message = format!(
                "snipe_public {reason}\nnft={nft} qty={qty} at={at:?} early_ms={early_ms} dry={dry}\n{}\n(session OpenSea API keys wiped)",
                reports.join("\n")
            );
            let _ = tx.send(JobDone {
                chat_id: chat,
                ok: reason == "SUCCESS",
                message,
            });
        });
        *live = Some(LiveJob {
            handle,
            gate: gate.clone(),
            chat_id: chat_id.to_string(),
        });
        Ok(format!(
            "WAITING → ARMED → FIRING\nmode=public nft={nft_ack} qty={qty} at={at:?} early_ms={early_ms} dry={dry}\nTelegram stays responsive — Cancel Session aborts countdown."
        ))
    } else {
        let handle = tokio::spawn(async move {
            gate_job.set_armed();
            let _ = gate_job.try_begin_firing();
            let message = match ops::run_public_snipe(&nft, qty, at, early_ms, dry).await {
                Ok(()) => {
                    gate_job.finish_success();
                    format!("snipe_public SUCCESS dry={dry} nft={nft} qty={qty} at={at:?} early_ms={early_ms}")
                }
                Err(e) => {
                    gate_job.finish_failed();
                    errclass::telegram_error(&e)
                }
            };
            let ok = message.contains("SUCCESS");
            let _ = tx.send(JobDone {
                chat_id: chat,
                ok,
                message,
            });
        });
        *live = Some(LiveJob {
            handle,
            gate: gate.clone(),
            chat_id: chat_id.to_string(),
        });
        Ok(format!(
            "WAITING (env wallet)\nmode=public nft={nft_ack} qty={qty} at={at:?} early_ms={early_ms} dry={dry}"
        ))
    }
}

/// Parse optional `[at|auto] [early_ms] [dry]` after slug/nft + qty.
fn parse_snipe_tail(tail: &[&str]) -> Result<(Option<i64>, i64, bool)> {
    if tail.is_empty() {
        return Ok((None, 50, false));
    }
    let first = tail[0];
    let is_dry = |s: &str| s == "dry" || s == "1" || s.eq_ignore_ascii_case("true");

    let (at, rest): (Option<i64>, &[&str]) = if first.eq_ignore_ascii_case("auto") {
        (None, &tail[1..])
    } else if is_dry(first) {
        return Ok((None, 50, true));
    } else if let Ok(n) = first.parse::<i64>() {
        if n.abs() < 1_000_000 {
            // small → early_ms with auto time
            let dry = tail.get(1).map(|s| is_dry(s)).unwrap_or(false);
            return Ok((None, n, dry));
        }
        (Some(crate::timing::parse_go_time(first)?), &tail[1..])
    } else {
        (Some(crate::timing::parse_go_time(first)?), &tail[1..])
    };

    if rest.first().map(|s| is_dry(s)).unwrap_or(false) {
        return Ok((at, 50, true));
    }
    let early_ms: i64 = rest.first().and_then(|s| s.parse().ok()).unwrap_or(50);
    let dry = rest.get(1).map(|s| is_dry(s)).unwrap_or(false);
    Ok((at, early_ms, dry))
}


fn feature_button_text(button: &str) -> String {
    match button {
        "🎨 mint nft" => "Mint NFT\nUse: /mint <slug> <qty>".into(),
        "🎯 snipe" => "Snipe\nUse: /snipe_wl <slug> <qty> [at|auto] [early_ms] [dry]".into(),
        "📦 batch mint" => "Batch Mint\nUse: /batch_mint <slug:qty,slug:qty,...>".into(),
        "🎯 batch snipe" => "Batch Snipe\nUse: /batch_snipe <slug:qty:at,slug:qty:at,...>".into(),
        "🔧 manual mint" => "Manual Mint\nUse: /manual_mint <contract> <value_eth> <calldata_hex> CONFIRM".into(),
        "🎛️ exec" => "Exec\nUse: /exec <to> <value_eth> <calldata_hex> CONFIRM".into(),
        "🎯 my snipes" => telegram_tools::my_snipes(),
        "📤 send nfts" => "Send NFTs\nUse: /send_nft <erc721|erc1155> ...".into(),
        "📤 batch send" => "Batch Send\nUse: /batch_send <erc721|erc1155> ...".into(),
        "🔥 burn nfts" => "Burn NFTs\nUse: /burn <contract> <token_id> CONFIRM".into(),
        "🏦 consolidate" => "Consolidate\nUse: /consolidate <target> [wallets|all] CONFIRM".into(),
        "🔍 eligibility" => "Eligibility\nUse: /eligibility <slug> [wallet_index]".into(),
        "💸 disperse eth" => "Disperse ETH\nUse: /disperse_eth <amount_each_eth> <target1,target2,...> CONFIRM".into(),
        "💸 send eth" => "Send ETH\nUse: /send_eth <to> <amount_eth> CONFIRM".into(),
        "👛 wallets" => "/wallets".into(),
        "🌐 rpc" => "/rpc".into(),
        "⚙️ settings" => telegram_tools::feature_help(),
        _ => telegram_tools::feature_help(),
    }
}

fn active_wallet(sess: &SnipeSession) -> Result<session::WalletEntry> {
    let wallets = session::load_available_wallets()?;
    if let Some(&idx) = sess.selected.first() {
        return wallets.get(idx).cloned().ok_or_else(|| eyre::eyre!("selected wallet missing"));
    }
    wallets.into_iter().next().ok_or_else(|| eyre::eyre!("no wallets configured"))
}

fn active_api_key(sess: &SnipeSession) -> Result<String> {
    let wallets = session::load_available_wallets()?;
    if let Some(&idx) = sess.selected.first() {
        if let Some(w) = wallets.get(idx) {
            if let Some(k) = sess.key_for(&w.address) {
                return Ok(k.to_string());
            }
        }
    }
    env::var("OPENSEA_API_KEY").wrap_err("no OpenSea API key available; use Snipe Setup or set OPENSEA_API_KEY")
}

fn require_confirm(parts: &[&str]) -> Result<()> {
    if parts.last().map(|s| *s == "CONFIRM").unwrap_or(false) {
        Ok(())
    } else {
        eyre::bail!("add CONFIRM at the end of the command");
    }
}

async fn feature_mint(parts: Vec<&str>, sess: &SnipeSession) -> Result<String> {
    if parts.len() < 3 { return Ok("usage: /mint <slug> <qty>".into()); }
    let wallet = active_wallet(sess)?;
    let key = active_api_key(sess)?;
    telegram_tools::mint(&key, &wallet, parts[1], parts[2].parse().wrap_err("qty")?).await
}

async fn feature_batch_mint(parts: Vec<&str>, sess: &SnipeSession) -> Result<String> {
    if parts.len() < 2 { return Ok("usage: /batch_mint <slug:qty,slug:qty,...>".into()); }
    let wallet = active_wallet(sess)?;
    let key = active_api_key(sess)?;
    let mut items = Vec::new();
    for item in parts[1].split(',') {
        let mut p = item.split(':');
        let slug = p.next().unwrap_or("").trim();
        let qty: u64 = p.next().ok_or_else(|| eyre::eyre!("missing qty in {item}"))?.parse().wrap_err("qty")?;
        if slug.is_empty() { eyre::bail!("empty slug"); }
        items.push((slug.to_string(), qty));
    }
    telegram_tools::batch_mint(&key, &wallet, &items).await
}

async fn feature_batch_snipe(parts: Vec<&str>, sess: &SnipeSession) -> Result<String> {
    if parts.len() < 2 { return Ok("usage: /batch_snipe <slug:qty:at,slug:qty:at,...>".into()); }
    let wallet = active_wallet(sess)?;
    let key = active_api_key(sess)?;
    let mut items = Vec::new();
    for item in parts[1].split(',') {
        let mut p = item.split(':');
        let slug = p.next().unwrap_or("").trim();
        let qty: u64 = p.next().ok_or_else(|| eyre::eyre!("missing qty in {item}"))?.parse().wrap_err("qty")?;
        let at_raw = p.next().unwrap_or("auto");
        let at = if at_raw.eq_ignore_ascii_case("auto") { None } else { Some(crate::timing::parse_go_time(at_raw)?) };
        if slug.is_empty() { eyre::bail!("empty slug"); }
        items.push((slug.to_string(), qty, at));
    }
    telegram_tools::batch_snipe(&key, &wallet, &items, 50, false).await
}

async fn feature_manual_mint(parts: Vec<&str>, sess: &SnipeSession) -> Result<String> {
    if parts.len() < 5 { return Ok("usage: /manual_mint <contract> <value_eth> <calldata_hex> CONFIRM".into()); }
    require_confirm(&parts)?;
    let wallet = active_wallet(sess)?;
    telegram_tools::manual_mint(&wallet, parts[1], parts[2], parts[3]).await
}

async fn feature_exec(parts: Vec<&str>, sess: &SnipeSession) -> Result<String> {
    if parts.len() < 5 { return Ok("usage: /exec <to> <value_eth> <calldata_hex> CONFIRM".into()); }
    require_confirm(&parts)?;
    let wallet = active_wallet(sess)?;
    telegram_tools::exec(&wallet, parts[1], parts[2], parts[3]).await
}

async fn feature_send_nft(parts: Vec<&str>, sess: &SnipeSession) -> Result<String> {
    if parts.len() < 5 { return Ok("usage: /send_nft <erc721|erc1155> <contract> <to> <token_id> [amount] [safe]".into()); }
    let wallet = active_wallet(sess)?;
    match parts[1].to_ascii_lowercase().as_str() {
        "erc721" | "721" => telegram_tools::send_nft_erc721(&wallet, parts[2], parts[3], parts[4].parse().wrap_err("token_id")?, parts.get(5).map(|s| s.eq_ignore_ascii_case("safe")).unwrap_or(false)).await,
        "erc1155" | "1155" => {
            if parts.len() < 6 { return Ok("usage: /send_nft erc1155 <contract> <to> <token_id> <amount>".into()); }
            telegram_tools::send_nft_erc1155(&wallet, parts[2], parts[3], parts[4].parse().wrap_err("token_id")?, parts[5].parse().wrap_err("amount")?).await
        }
        _ => Ok("type must be erc721 or erc1155".into()),
    }
}

async fn feature_batch_send(parts: Vec<&str>, sess: &SnipeSession) -> Result<String> {
    if parts.len() < 4 { return Ok("usage: /batch_send erc721 <to> <contract:token,contract:token,...>".into()); }
    let wallet = active_wallet(sess)?;
    let typ = parts[1].to_ascii_lowercase();
    let to = parts[2];
    let mut lines = Vec::new();
    for item in parts[3].split(',') {
        let p: Vec<&str> = item.split(':').collect();
        if typ == "erc721" || typ == "721" {
            if p.len() != 2 { return Ok("ERC721 batch item format: contract:token_id".into()); }
            lines.push(telegram_tools::send_nft_erc721(&wallet, p[0], to, p[1].parse().wrap_err("token_id")?, false).await?);
        } else {
            if p.len() != 3 { return Ok("ERC1155 batch item format: contract:token_id:amount".into()); }
            lines.push(telegram_tools::send_nft_erc1155(&wallet, p[0], to, p[1].parse().wrap_err("token_id")?, p[2].parse().wrap_err("amount")?).await?);
        }
    }
    Ok(lines.join("\n"))
}

async fn feature_burn(parts: Vec<&str>, sess: &SnipeSession) -> Result<String> {
    if parts.len() < 4 { return Ok("usage: /burn <contract> <token_id> CONFIRM".into()); }
    require_confirm(&parts)?;
    let wallet = active_wallet(sess)?;
    telegram_tools::burn_erc721(&wallet, parts[1], parts[2].parse().wrap_err("token_id")?).await
}

async fn feature_consolidate(parts: Vec<&str>, sess: &SnipeSession) -> Result<String> {
    if parts.len() < 3 { return Ok("usage: /consolidate <target> [wallets|all] CONFIRM".into()); }
    require_confirm(&parts)?;
    let target = alloy::primitives::Address::from_str(parts[1]).wrap_err("target")?;
    let wallets = session::load_available_wallets()?;
    let selected = if parts[2].eq_ignore_ascii_case("all") {
        wallets
    } else {
        let idxs = session::parse_wallet_selection(parts[2], wallets.len())?;
        idxs.into_iter().filter_map(|i| wallets.get(i - 1).cloned()).collect()
    };
    telegram_tools::consolidate(&selected, target).await
}

async fn feature_eligibility(parts: Vec<&str>, sess: &SnipeSession) -> Result<String> {
    if parts.len() < 2 { return Ok("usage: /eligibility <slug> [wallet_index]".into()); }
    let wallets = session::load_available_wallets()?;
    let wallet = if let Some(raw) = parts.get(2) {
        let i: usize = raw.parse().wrap_err("wallet index")?;
        wallets.get(i.saturating_sub(1)).cloned().ok_or_else(|| eyre::eyre!("wallet index out of range"))?
    } else {
        active_wallet(sess)?
    };
    let key = active_api_key(sess)?;
    telegram_tools::eligibility(&key, &wallet, parts[1], 1).await
}

async fn feature_disperse_eth(parts: Vec<&str>, sess: &SnipeSession) -> Result<String> {
    if parts.len() < 4 { return Ok("usage: /disperse_eth <amount_each_eth> <target1,target2,...> CONFIRM".into()); }
    require_confirm(&parts)?;
    let wallet = active_wallet(sess)?;
    let targets = parts[2].split(',').map(alloy::primitives::Address::from_str).collect::<std::result::Result<Vec<_>,_>>().wrap_err("target address")?;
    telegram_tools::disperse_eth(&wallet, parts[1], &targets).await
}

async fn feature_send_eth(parts: Vec<&str>, sess: &SnipeSession) -> Result<String> {
    if parts.len() < 4 { return Ok("usage: /send_eth <to> <amount_eth> CONFIRM".into()); }
    require_confirm(&parts)?;
    let wallet = active_wallet(sess)?;
    telegram_tools::send_eth(&wallet, parts[1], parts[2]).await
}

fn help_text() -> String {
    r#"Commands (unlocked chat only — /password first):
/start /password <code> /lock|/logout
Snipe Setup — select wallet(s) → paste NEW OpenSea API key → optional API name → Arm
Arm — show mint params once session keys are attached
Cancel Session — abort WAITING/ARMED/FIRING + wipe temporary OpenSea API keys

/import_wallet <pk> — import wallet, then "Set your wallet name"
/rename_wallet <index|label|addr> <name>
/rename_api <selected_index|label|addr> <name>  (session label only)
/wallets /session /status /doctor /help
/rpc /rpc_set /rpc_add /rpc_clear_extra /rank
/snipe_public <nft> <qty> [at|auto] [early_ms] [dry]
/snipe_wl <slug> <qty> [at|auto] [early_ms] [dry]
(omit at or auto → mint-window auto-detect)


Session API keys are wiped after SUCCESS / FAILED / TIMEOUT / CANCELLED.
Wallet private keys + display names persist. No permanent API_1 vault.
Private keys / full API keys / password are never logged or sent."#
        .into()
}

fn status_text(sess: &SnipeSession) -> String {
    // TaskGate is process-global via countdown cancel; phase still shown from session.
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
