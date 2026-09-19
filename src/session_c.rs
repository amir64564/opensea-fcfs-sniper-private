
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
            let label = names.get(&format!("{addr}")).cloned().unwrap_or(label);
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
