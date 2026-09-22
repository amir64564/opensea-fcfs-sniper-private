impl SnipeSession {
/// Rename API label for a session key (by wallet index 1-based among selected, or address).
    pub fn rename_api(
        &mut self,
        available: &[WalletEntry],
        target: &str,
        new_name: &str,
    ) -> Result<String> {
        if self.keys.is_empty() {
            eyre::bail!("no session API keys — run Snipe Setup and paste a key first");
        }
        let addr = resolve_selected_target(available, &self.selected, target)?;
        if !self.keys.contains_key(&addr) {
            eyre::bail!("no session API key for that wallet");
        }
        let n = sanitize_name(new_name)?;
        self.api_labels.insert(addr, n.clone());
        self.persist()?;
        Ok(format!(
            "API name updated (session): \"{n}\" → {}",
            short_addr(&addr)
        ))
    }

    pub fn mark_running(&mut self) {
        self.phase = Phase::Running;
    }

    pub fn mark_ready(&mut self) {
        if !self.keys.is_empty() && !self.selected.is_empty() {
            self.phase = Phase::ReadyToArm;
        }
    }

    pub fn begin_await_wallet_name(&mut self, address: Address) {
        self.phase = Phase::AwaitWalletName { address };
    }

    pub fn key_for(&self, addr: &Address) -> Option<&str> {
        self.keys.get(addr).map(|s| s.as_str())
    }

    pub fn api_label_for(&self, addr: &Address) -> Option<&str> {
        self.api_labels.get(addr).map(|s| s.as_str())
    }

    /// Ordered (wallet, api_key) pairs for the live session.
    pub fn wallet_key_pairs(
        &self,
        available: &[WalletEntry],
    ) -> Result<Vec<(WalletEntry, String)>> {
        let mut out = Vec::new();
        for &idx in &self.selected {
            let w = available
                .get(idx)
                .ok_or_else(|| eyre::eyre!("selected wallet index {idx} missing"))?
                .clone();
            let key =
                self.keys.get(&w.address).cloned().ok_or_else(|| {
                    eyre::eyre!("no session API key for {}", short_addr(&w.address))
                })?;
            out.push((w, key));
        }
        Ok(out)
    }

    pub fn map_summary(&self, available: &[WalletEntry]) -> String {
        let mut lines = Vec::new();
        for &idx in &self.selected {
            let Some(w) = available.get(idx) else {
                continue;
            };
            let masked = self
                .keys
                .get(&w.address)
                .map(|k| mask_api_key(k))
                .unwrap_or_else(|| "(missing)".into());
            let api_name = self
                .api_labels
                .get(&w.address)
                .map(|s| format!(" \"{s}\""))
                .unwrap_or_default();
            lines.push(format!("  {} → {}{api_name}", display_wallet(w), masked));
        }
        if lines.is_empty() {
            "(empty)".into()
        } else {
            lines.join("\n")
        }
    }

    /// Wipe temporary OpenSea API keys + session API labels. Does NOT touch wallets / wallet names.
    pub fn cleanup(&mut self, reason: &str) {
        crate::outln!(
            "session cleanup reason={reason} (OpenSea API keys wiped; wallet names/keys untouched)"
        );
        self.wipe_keys_only();
        self.selected.clear();
        self.ui_mode = None;
        self.ui_target = None;
        self.ui_qty = 1;
        self.ui_early_ms = 3000;
        // If we were mid wallet-name after import, leave Idle (name save should finish first).
        self.phase = Phase::Idle;
    }

    fn wipe_keys_only(&mut self) {
        for (_addr, mut key) in self.keys.drain() {
            unsafe {
                let v = key.as_mut_vec();
                for b in v.iter_mut() {
                    *b = 0;
                }
                v.clear();
            }
        }
        self.api_labels.clear();
        let _ = fs::remove_file(&self.file_path);
    }

    fn persist(&self) -> Result<()> {
        let mut file = SessionKeyFile::default();
        for (addr, key) in &self.keys {
            file.keys.insert(format!("{addr}"), key.clone());
        }
        for (addr, label) in &self.api_labels {
            file.api_labels.insert(format!("{addr}"), label.clone());
        }
        let json = serde_json::to_vec(&file).wrap_err("serialize session")?;
        if let Some(parent) = self.file_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&self.file_path)
            .wrap_err("open session file")?;
        f.write_all(&json).wrap_err("write session file")?;
        f.sync_all().ok();
        Ok(())
    }
}

fn session_file_path() -> PathBuf {
    let pid = std::process::id();
    PathBuf::from(format!("/tmp/opensea-fcfs-sniper-oskeys-{pid}.json"))
}

pub fn wallet_names_path() -> &'static str {
    "wallet_names.json"
}

pub fn wallets_json_path() -> &'static str {
    "wallets.json"
}

pub fn mask_api_key(key: &str) -> String {
    let k = key.trim();
    if k.len() <= 8 {
        return "****".into();
    }
    format!(
        "{}…{}",
        &k[..4.min(k.len())],
        &k[k.len().saturating_sub(4)..]
    )
}

pub fn short_addr(addr: &Address) -> String {
    let s = format!("{addr}");
    if s.len() > 12 {
        format!("{}…{}", &s[..6], &s[s.len() - 4..])
    } else {
        s
    }
}

pub fn display_wallet(w: &WalletEntry) -> String {
    if w.label.is_empty() {
        short_addr(&w.address)
    } else {
        format!("{} ({})", w.label, short_addr(&w.address))
    }
}

fn format_selected(available: &[WalletEntry], selected: &[usize]) -> String {
    selected
        .iter()
        .enumerate()
        .filter_map(|(i, &idx)| {
            available
                .get(idx)
                .map(|w| format!("  {}. {}", i + 1, display_wallet(w)))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sanitize_name(name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() {
        eyre::bail!("name cannot be empty (or send /skip)");
    }
    if n.starts_with('/') {
        eyre::bail!("name cannot start with /");
    }
    if n.len() > 48 {
        eyre::bail!("name too long (max 48)");
    }
    if n.chars().any(|c| c == '\n' || c == '\r' || c == '\0') {
        eyre::bail!("name has invalid characters");
    }
    Ok(n.to_string())
}

fn resolve_selected_target(
    available: &[WalletEntry],
    selected: &[usize],
    target: &str,
) -> Result<Address> {
    let t = target.trim();
    if t.is_empty() {
        eyre::bail!("usage: provide wallet index or address");
    }
    if let Ok(i) = t.parse::<usize>() {
        if i == 0 || i > selected.len() {
            eyre::bail!("invalid selected index {i} (1..{})", selected.len());
        }
        return Ok(available[selected[i - 1]].address);
    }
    // match by label among selected
    let lower = t.to_lowercase();
    for &idx in selected {
        let w = &available[idx];
        if w.label.to_lowercase() == lower {
            return Ok(w.address);
        }
    }
    let addr = Address::from_str(t).wrap_err("expected index, label, or address")?;
    Ok(addr)
}

/// Load wallets from WALLET_KEY (single) and optional wallets.json (multi).
/// Display names from wallets.json label field and/or wallet_names.json (address → name).
pub fn load_available_wallets() -> Result<Vec<WalletEntry>> {
    let mut out: Vec<WalletEntry> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let names = load_wallet_names();

    if let Ok(key) = std::env::var("WALLET_KEY") {
        let key = key.trim().to_string();
        if !key.is_empty() {
            let signer: PrivateKeySigner = key.parse().wrap_err("invalid WALLET_KEY")?;
            let addr = signer.address();
            if seen.insert(addr) {
                let label = names
                    .get(&format!("{addr}"))
                    .cloned()
                    .unwrap_or_else(|| "env".into());
                out.push(WalletEntry {
                    address: addr,
                    private_key: key,
                    label,
                });
            }
        }
    }

    for path in [wallets_json_path(), "wallets/wallets.json"] {
        if Path::new(path).exists() {
            append_wallets_file(path, &mut out, &mut seen, &names)?;
            break;
        }
    }

    // Apply wallet_names.json overrides (address is id)
    for w in &mut out {
        if let Some(n) = names.get(&format!("{}", w.address)) {
            w.label = n.clone();
        }
    }

    if out.is_empty() {
        eyre::bail!("no wallets: set WALLET_KEY in .env and/or provide wallets.json");
    }
    Ok(out)
}
