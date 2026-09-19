
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
