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
