
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
