
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
