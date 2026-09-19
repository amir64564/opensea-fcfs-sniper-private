
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
    if parts.len() < 3 {
        return Ok("usage: /snipe_wl <slug> <qty> [at|auto] [early_ms] [dry]\n(omit at or use auto → OpenSea stage startTime)".into());
    }
    let slug = parts[1].to_string();
    let qty: u64 = parts[2].parse().wrap_err("qty")?;
    let (at, early_ms, dry) = parse_snipe_tail(&parts[3..])?;

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
            "telegram snipe_wl session wallets={} slug={slug} qty={qty} at={at:?} early_ms={early_ms} dry={dry}",
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
            "snipe_wl {reason}\nslug={slug} qty={qty} at={at:?} early_ms={early_ms} dry={dry}\n{}\n(session OpenSea API keys wiped)",
            reports.join("\n")
        ))
    } else {
        // Fallback: env OPENSEA_API_KEY + WALLET_KEY (no session)
        crate::outln!("telegram snipe_wl (env keys) slug={slug} qty={qty} at={at:?} early_ms={early_ms} dry={dry}");
        match ops::run_api_snipe(&slug, qty, at, early_ms, dry).await {
            Ok(()) => Ok(format!(
                "snipe_wl done dry={dry} slug={slug} qty={qty} at={at:?} early_ms={early_ms}"
            )),
            Err(e) => Err(e),
        }
    }
}

async fn run_snipe_public(parts: Vec<&str>, sess: &mut SnipeSession) -> Result<String> {
    if parts.len() < 3 {
        return Ok("usage: /snipe_public <nft> <qty> [at|auto] [early_ms] [dry]\n(omit at or use auto → on-chain getPublicDrop startTime)".into());
    }
    let nft = parts[1].to_string();
    let qty: u64 = parts[2].parse().wrap_err("qty")?;
    let (at, early_ms, dry) = parse_snipe_tail(&parts[3..])?;

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
            "snipe_public {reason}\nnft={nft} qty={qty} at={at:?} early_ms={early_ms} dry={dry}\n{}\n(session OpenSea API keys wiped)",
            reports.join("\n")
        ))
    } else {
        crate::outln!("telegram snipe_public nft={nft} qty={qty} at={at:?} early_ms={early_ms} dry={dry}");
        ops::run_public_snipe(&nft, qty, at, early_ms, dry).await?;
        Ok(format!(
            "snipe_public done dry={dry} nft={nft} qty={qty} at={at:?} early_ms={early_ms}"
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

fn help_text() -> String {
    r#"Commands (unlocked chat only — /password first):
/start /password <code> /lock|/logout
Snipe Setup — select wallet(s) → paste NEW OpenSea API key → optional API name → Arm
Arm — show mint params once session keys are attached
Cancel Session — wipe temporary OpenSea API keys

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
