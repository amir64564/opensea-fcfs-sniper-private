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
    AwaitApiKey {
        wallet_idx: usize,
    },
    /// Optional: "Set your API name" for the key just attached (session label only).
    AwaitApiName {
        wallet_idx: usize,
    },
    /// After /import_wallet: set persistent display name.
    AwaitWalletName {
        address: Address,
    },
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

}
