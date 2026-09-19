
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
