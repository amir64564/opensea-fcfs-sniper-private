//! Actionable, secret-safe error classification for Telegram / logs.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrKind {
    Config,
    Rpc,
    OpenSea,
    Tx,
    Funds,
    Timeout,
    SoldOut,
    Already,
    Cancelled,
    Unknown,
}

impl ErrKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Rpc => "rpc",
            Self::OpenSea => "opensea",
            Self::Tx => "tx",
            Self::Funds => "funds",
            Self::Timeout => "timeout",
            Self::SoldOut => "soldout",
            Self::Already => "already",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
        }
    }
}

/// Classify an error message (already stringified). Never returns secrets.
pub fn classify(msg: &str) -> ErrKind {
    let m = msg.to_ascii_lowercase();
    if m.contains("cancel") {
        return ErrKind::Cancelled;
    }
    if m.contains("timeout") || m.contains("timed out") || m.contains("deadline") {
        return ErrKind::Timeout;
    }
    if m.contains("sold out")
        || m.contains("soldout")
        || m.contains("no supply")
        || m.contains("max supply")
        || m.contains("insufficient supply")
    {
        return ErrKind::SoldOut;
    }
    if m.contains("already minted")
        || m.contains("already claimed")
        || m.contains("already known")
        || m.contains("nonce too low")
        || m.contains("already participating")
    {
        return ErrKind::Already;
    }
    if m.contains("insufficient funds")
        || m.contains("insufficient balance")
        || m.contains("exceeds balance")
        || m.contains("gas required exceeds")
    {
        return ErrKind::Funds;
    }
    if m.contains("opensea")
        || m.contains("api key")
        || m.contains("x-api-key")
        || m.contains("401")
        || m.contains("403")
        || m.contains("drop slug")
        || m.contains("mint hammer")
    {
        return ErrKind::OpenSea;
    }
    if m.contains("rpc")
        || m.contains("json-rpc")
        || m.contains("connection refused")
        || m.contains("dns")
        || m.contains("broadcast")
        || m.contains("eth_send")
    {
        return ErrKind::Rpc;
    }
    if m.contains("wallet_key")
        || m.contains("rpc_url")
        || m.contains("missing")
        || m.contains("invalid")
        || m.contains("parse")
        || m.contains("config")
    {
        return ErrKind::Config;
    }
    if m.contains("revert") || m.contains("execution reverted") || m.contains("tx") {
        return ErrKind::Tx;
    }
    ErrKind::Unknown
}

/// Redact likely secrets from an error string for Telegram.
pub fn sanitize(msg: &str) -> String {
    let mut s = msg.to_string();
    // 0x + 64 hex private keys
    let bytes = s.clone().into_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 66 <= bytes.len()
            && bytes[i] == b'0'
            && bytes[i + 1] == b'x'
            && bytes[i + 2..i + 66].iter().all(|c| c.is_ascii_hexdigit())
        {
            out.push_str("0x[redacted]");
            i += 66;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    s = out;
    // Long hex/base64-ish tokens (API keys) — keep short prefix
    if let Ok(key) = std::env::var("OPENSEA_API_KEY") {
        if key.len() >= 8 {
            s = s.replace(&key, "[api_key]");
        }
    }
    if let Ok(key) = std::env::var("WALLET_KEY") {
        if key.len() >= 8 {
            s = s.replace(&key, "[wallet_key]");
        }
    }
    if let Ok(tok) = std::env::var("TELEGRAM_BOT_TOKEN") {
        if !tok.is_empty() {
            s = s.replace(&tok, "[bot_token]");
        }
    }
    if let Ok(pw) = std::env::var("TELEGRAM_BOT_PASSWORD") {
        if pw.len() >= 4 {
            s = s.replace(&pw, "[password]");
        }
    }
    // Truncate noisy bodies
    if s.len() > 500 {
        s = format!("{}…", &s[..500]);
    }
    s
}

/// One-line Telegram-friendly error.
pub fn telegram_error(err: &eyre::Report) -> String {
    let raw = format!("{err:#}");
    let kind = classify(&raw);
    let safe = sanitize(&raw);
    format!("[{}] {}", kind.as_str(), safe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_cases() {
        assert_eq!(classify("OpenSea mint hammer timed out"), ErrKind::Timeout);
        assert_eq!(classify("sold out of supply"), ErrKind::SoldOut);
        assert_eq!(classify("insufficient funds for gas"), ErrKind::Funds);
        assert_eq!(classify("all RPC broadcasts failed"), ErrKind::Rpc);
        assert_eq!(classify("WALLET_KEY missing"), ErrKind::Config);
        assert_eq!(classify("already minted"), ErrKind::Already);
        assert_eq!(classify("task cancelled by user"), ErrKind::Cancelled);
    }

    #[test]
    fn sanitize_strips_privkey() {
        let pk = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let s = sanitize(&format!("boom signer={pk}"));
        assert!(!s.contains("aaaa"));
        assert!(s.contains("[redacted]"));
    }
}
