
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
