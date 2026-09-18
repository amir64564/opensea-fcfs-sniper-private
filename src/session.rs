//! Temporary OpenSea API-key session for Telegram snipe setup.
//!
//! Flow: Idle → PickWallets → AwaitApiKey → AwaitApiName (optional label) → ReadyToArm → Running → Cleanup
//! Wallet private keys + display names PERSIST (wallets.json / wallet_names.json).
//! OpenSea API key SECRET is session-only (memory + 0600 /tmp); wiped on end.
//! Optional API display name is UI-only for the live session — NOT a permanent vault.

use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Session state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Idle,
    PickWallets,
    /// Waiting for a NEW OpenSea API key paste for selected[wallet_idx].
    AwaitApiKey { wallet_idx: usize },
    /// Optional: "Set your API name" for the key just attached (session label only).
    AwaitApiName { wallet_idx: usize },
    /// After /import_wallet: set persistent display name.
    AwaitWalletName { address: Address },
    ReadyToArm,
    Running,
}

#[derive(Debug, Clone)]
pub struct WalletEntry {
    pub address: Address,
    /// Private key hex (0x…) — from WALLET_KEY / wallets.json only.
    pub private_key: String,
    /// Persistent display name (address remains the id).
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SessionKeyFile {
    /// address hex → OpenSea API key (session-only secrets)
    keys: HashMap<String, String>,
    /// address hex → optional session UI label for the API key (not the secret)
    api_labels: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct WalletNamesFile {
    /// address hex → display name
    names: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WalletsFile {
    #[serde(default)]
    version: u32,
    wallets: Vec<WalletFileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WalletFileEntry {
    private_key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    quantity: Option<u64>,
}

pub struct SnipeSession {
    pub phase: Phase,
    /// Indices into `available` that the user selected (order preserved).
    pub selected: Vec<usize>,
    /// address → temporary OpenSea API key for this session only
    keys: HashMap<Address, String>,
    /// address → optional session-only API display name (UI; wiped with session)
    api_labels: HashMap<Address, String>,
    file_path: PathBuf,
}

impl Default for SnipeSession {
    fn default() -> Self {
        Self::new()
    }
}

impl SnipeSession {
    pub fn new() -> Self {
        Self {
            phase: Phase::Idle,
            selected: Vec::new(),
            keys: HashMap::new(),
            api_labels: HashMap::new(),
            file_path: session_file_path(),
        }
    }

    pub fn is_idle(&self) -> bool {
        matches!(self.phase, Phase::Idle)
    }

    pub fn is_awaiting_api_key(&self) -> bool {
        matches!(self.phase, Phase::AwaitApiKey { .. })
    }

    pub fn is_awaiting_api_name(&self) -> bool {
        matches!(self.phase, Phase::AwaitApiName { .. })
    }

    pub fn is_awaiting_wallet_name(&self) -> bool {
        matches!(self.phase, Phase::AwaitWalletName { .. })
    }

    pub fn is_picking_wallets(&self) -> bool {
        matches!(self.phase, Phase::PickWallets)
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.phase, Phase::ReadyToArm)
    }

    pub fn is_running(&self) -> bool {
        matches!(self.phase, Phase::Running)
    }

    pub fn has_selected(&self) -> bool {
        !self.selected.is_empty()
    }

    /// True when session has attached keys and is ready to mint (or mid-run).
    pub fn can_mint(&self) -> bool {
        !self.selected.is_empty()
            && !self.keys.is_empty()
            && matches!(self.phase, Phase::ReadyToArm | Phase::Running)
    }

    pub fn start_setup(&mut self) {
        self.wipe_keys_only();
        self.selected.clear();
        self.phase = Phase::PickWallets;
    }

    /// Select wallets by 1-based indices. Returns confirmation + prompt for first API key.
    pub fn select_wallets(
        &mut self,
        available: &[WalletEntry],
        indices_1based: &[usize],
    ) -> Result<String> {
        if available.is_empty() {
            eyre::bail!("no wallets configured (set WALLET_KEY and/or wallets.json)");
        }
        let mut selected = Vec::new();
        for &i in indices_1based {
            if i == 0 || i > available.len() {
                eyre::bail!("invalid wallet index {i} (valid 1..{})", available.len());
            }
            let idx = i - 1;
            if !selected.contains(&idx) {
                selected.push(idx);
            }
        }
        if selected.is_empty() {
            eyre::bail!("select at least one wallet (e.g. 1 or 1,2)");
        }
        self.selected = selected;
        self.keys.clear();
        self.api_labels.clear();
        self.phase = Phase::AwaitApiKey { wallet_idx: 0 };
        let w = &available[self.selected[0]];
        Ok(format!(
            "Selected wallet(s):\n{}\n\nSend the OpenSea API key for this wallet.\n{}",
            format_selected(available, &self.selected),
            display_wallet(w)
        ))
    }

    pub fn select_all(&mut self, available: &[WalletEntry]) -> Result<String> {
        let idxs: Vec<usize> = (1..=available.len()).collect();
        self.select_wallets(available, &idxs)
    }

    /// Attach a NEW API key to the current AwaitApiKey wallet, then prompt for optional API name.
    pub fn attach_api_key(&mut self, available: &[WalletEntry], raw_key: &str) -> Result<String> {
        let key = raw_key.trim();
        if key.is_empty() || key.len() < 8 {
            eyre::bail!("API key looks too short — paste a full OpenSea API key");
        }
        if key.starts_with('/') {
            eyre::bail!("that looks like a command, not an API key");
        }
        let Phase::AwaitApiKey { wallet_idx } = self.phase else {
            eyre::bail!("not waiting for an API key (phase={:?})", self.phase);
        };
        if wallet_idx >= self.selected.len() {
            eyre::bail!("internal: wallet_idx out of range");
        }
        let avail_idx = self.selected[wallet_idx];
        let w = available
            .get(avail_idx)
            .ok_or_else(|| eyre::eyre!("wallet missing"))?;
        self.keys.insert(w.address, key.to_string());
        self.persist()?;

        let masked = mask_api_key(key);
        self.phase = Phase::AwaitApiName { wallet_idx };
        Ok(format!(
            "Attached key {masked} → {} (session only).\n\nSet your API name (or send /skip):",
            display_wallet(w)
        ))
    }

    /// Set session-only API display name, then advance to next key or ReadyToArm.
    pub fn set_api_name(
        &mut self,
        available: &[WalletEntry],
        name: &str,
        skip: bool,
    ) -> Result<String> {
        let Phase::AwaitApiName { wallet_idx } = self.phase else {
            eyre::bail!("not waiting for an API name (phase={:?})", self.phase);
        };
        let avail_idx = self.selected[wallet_idx];
        let w = available
            .get(avail_idx)
            .ok_or_else(|| eyre::eyre!("wallet missing"))?;

        let mut msg = String::new();
        if !skip {
            let n = sanitize_name(name)?;
            self.api_labels.insert(w.address, n.clone());
            self.persist()?;
            msg.push_str(&format!(
                "API name saved (session): \"{n}\" → {}\n",
                display_wallet(w)
            ));
        } else {
            msg.push_str("API name skipped.\n");
        }

        if wallet_idx + 1 < self.selected.len() {
            let next = wallet_idx + 1;
            self.phase = Phase::AwaitApiKey { wallet_idx: next };
            let nw = &available[self.selected[next]];
            msg.push_str(&format!(
                "\nSend the OpenSea API key for this wallet.\n{}",
                display_wallet(nw)
            ));
        } else {
            self.phase = Phase::ReadyToArm;
            msg.push_str(&format!(
                "\n✅ Session ready. Press Arm or send mint params.\nMap:\n{}",
                self.map_summary(available)
            ));
        }
        Ok(msg)
    }

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
    pub fn wallet_key_pairs(&self, available: &[WalletEntry]) -> Result<Vec<(WalletEntry, String)>> {
        let mut out = Vec::new();
        for &idx in &self.selected {
            let w = available
                .get(idx)
                .ok_or_else(|| eyre::eyre!("selected wallet index {idx} missing"))?
                .clone();
            let key = self
                .keys
                .get(&w.address)
                .cloned()
                .ok_or_else(|| eyre::eyre!("no session API key for {}", short_addr(&w.address)))?;
            out.push((w, key));
        }
        Ok(out)
    }

    pub fn map_summary(&self, available: &[WalletEntry]) -> String {
        let mut lines = Vec::new();
        for &idx in &self.selected {
            let Some(w) = available.get(idx) else { continue };
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
            lines.push(format!(
                "  {} → {}{api_name}",
                display_wallet(w),
                masked
            ));
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

fn load_wallet_names() -> HashMap<String, String> {
    let path = wallet_names_path();
    if !Path::new(path).exists() {
        return HashMap::new();
    }
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<WalletNamesFile>(&text)
            .map(|f| f.names)
            .unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

fn save_wallet_names(names: &HashMap<String, String>) -> Result<()> {
    let file = WalletNamesFile {
        names: names.clone(),
    };
    let json = serde_json::to_vec_pretty(&file).wrap_err("serialize wallet_names")?;
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(wallet_names_path())
        .wrap_err("write wallet_names.json")?;
    f.write_all(&json)?;
    f.write_all(b"\n")?;
    Ok(())
}

/// Persist display name for a wallet address (does not change the address id).
pub fn set_wallet_display_name(address: Address, name: &str) -> Result<String> {
    let n = sanitize_name(name)?;
    let mut names = load_wallet_names();
    names.insert(format!("{address}"), n.clone());
    save_wallet_names(&names)?;

    // Also update label inside wallets.json if that wallet lives there.
    let _ = update_wallets_json_label(address, &n);

    Ok(format!(
        "Wallet name saved: \"{n}\" → {}",
        short_addr(&address)
    ))
}

/// Rename by index (1-based in available list), label, or address.
pub fn rename_wallet(available: &[WalletEntry], target: &str, new_name: &str) -> Result<String> {
    let addr = resolve_available_target(available, target)?;
    set_wallet_display_name(addr, new_name)
}

fn resolve_available_target(available: &[WalletEntry], target: &str) -> Result<Address> {
    let t = target.trim();
    if t.is_empty() {
        eyre::bail!("usage: /rename_wallet <index|label|address> <name>");
    }
    if let Ok(i) = t.parse::<usize>() {
        if i == 0 || i > available.len() {
            eyre::bail!("invalid wallet index {i} (1..{})", available.len());
        }
        return Ok(available[i - 1].address);
    }
    let lower = t.to_lowercase();
    for w in available {
        if w.label.to_lowercase() == lower {
            return Ok(w.address);
        }
    }
    Address::from_str(t).wrap_err("expected index, label, or address")
}

fn update_wallets_json_label(address: Address, name: &str) -> Result<()> {
    let path = wallets_json_path();
    if !Path::new(path).exists() {
        return Ok(());
    }
    let text = fs::read_to_string(path)?;
    let mut v: serde_json::Value = serde_json::from_str(&text)?;
    let arr = if let Some(a) = v.get_mut("wallets").and_then(|w| w.as_array_mut()) {
        a
    } else if let Some(a) = v.as_array_mut() {
        a
    } else {
        return Ok(());
    };
    for e in arr.iter_mut() {
        let pk = e
            .get("private_key")
            .or_else(|| e.get("key"))
            .and_then(|x| x.as_str());
        let Some(pk) = pk else { continue };
        if let Ok(signer) = pk.parse::<PrivateKeySigner>() {
            if signer.address() == address {
                if let Some(obj) = e.as_object_mut() {
                    obj.insert("label".into(), serde_json::Value::String(name.to_string()));
                }
            }
        }
    }
    let out = serde_json::to_vec_pretty(&v)?;
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(&out)?;
    f.write_all(b"\n")?;
    Ok(())
}

/// Import a private key into wallets.json; returns address. Caller should prompt for name.
pub fn import_wallet(private_key: &str) -> Result<Address> {
    let pk = private_key.trim();
    if pk.is_empty() {
        eyre::bail!("empty private key");
    }
    let signer: PrivateKeySigner = pk.parse().wrap_err("invalid private key")?;
    let addr = signer.address();

    let path = wallets_json_path();
    let mut file = if Path::new(path).exists() {
        let text = fs::read_to_string(path)?;
        serde_json::from_str::<WalletsFile>(&text).unwrap_or(WalletsFile {
            version: 1,
            wallets: Vec::new(),
        })
    } else {
        WalletsFile {
            version: 1,
            wallets: Vec::new(),
        }
    };

    // Dedup by address
    for w in &file.wallets {
        if let Ok(s) = w.private_key.parse::<PrivateKeySigner>() {
            if s.address() == addr {
                eyre::bail!("wallet {} already imported", short_addr(&addr));
            }
        }
    }

    file.wallets.push(WalletFileEntry {
        private_key: pk.to_string(),
        label: String::new(),
        quantity: None,
    });

    let json = serde_json::to_vec_pretty(&file)?;
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .wrap_err("write wallets.json")?;
    f.write_all(&json)?;
    f.write_all(b"\n")?;
    Ok(addr)
}

fn append_wallets_file(
    path: &str,
    out: &mut Vec<WalletEntry>,
    seen: &mut std::collections::HashSet<Address>,
    names: &HashMap<String, String>,
) -> Result<()> {
    let text = fs::read_to_string(path).wrap_err_with(|| format!("read {path}"))?;
    let v: serde_json::Value = serde_json::from_str(&text).wrap_err("wallets.json json")?;

    let entries: Vec<serde_json::Value> = if let Some(arr) = v.as_array() {
        arr.clone()
    } else if let Some(arr) = v.get("wallets").and_then(|w| w.as_array()) {
        arr.clone()
    } else {
        eyre::bail!("{path}: expected array or {{wallets:[...]}}");
    };

    for (i, e) in entries.iter().enumerate() {
        let (pk, label) = match e {
            serde_json::Value::String(s) => (s.clone(), format!("w{i}")),
            serde_json::Value::Object(map) => {
                let pk = map
                    .get("private_key")
                    .or_else(|| map.get("key"))
                    .or_else(|| map.get("wallet_key"))
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| eyre::eyre!("{path} wallets[{i}]: missing private_key"))?
                    .to_string();
                let label = map
                    .get("label")
                    .or_else(|| map.get("name"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let label = if label.is_empty() {
                    format!("w{i}")
                } else {
                    label
                };
                (pk, label)
            }
            _ => eyre::bail!("{path} wallets[{i}]: expected string or object"),
        };
        let pk = pk.trim().to_string();
        if pk.is_empty() {
            continue;
        }
        let signer: PrivateKeySigner = pk
            .parse()
            .wrap_err_with(|| format!("{path} wallets[{i}]: invalid private key"))?;
        let addr = signer.address();
        if seen.insert(addr) {
            let label = names
                .get(&format!("{addr}"))
                .cloned()
                .unwrap_or(label);
            out.push(WalletEntry {
                address: addr,
                private_key: pk,
                label,
            });
        }
    }
    Ok(())
}

pub fn format_wallet_list(wallets: &[WalletEntry]) -> String {
    if wallets.is_empty() {
        return "No wallets configured.".into();
    }
    let mut lines = vec![format!(
        "Wallets ({}) — reply with number(s), e.g. 1 or 1,2 or all:",
        wallets.len()
    )];
    for (i, w) in wallets.iter().enumerate() {
        lines.push(format!("  {}. {}", i + 1, display_wallet(w)));
    }
    lines.join("\n")
}

/// Parse "1", "1,2", "1 2", "all"
pub fn parse_wallet_selection(text: &str, n: usize) -> Result<Vec<usize>> {
    let t = text.trim().to_lowercase();
    if t == "all" || t == "*" {
        return Ok((1..=n).collect());
    }
    let mut idxs = Vec::new();
    for part in t.split(|c: char| c == ',' || c.is_whitespace()) {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        let i: usize = p
            .parse()
            .wrap_err_with(|| format!("bad wallet index '{p}'"))?;
        idxs.push(i);
    }
    if idxs.is_empty() {
        eyre::bail!("reply with wallet number(s), e.g. 1 or 1,2 or all");
    }
    Ok(idxs)
}
