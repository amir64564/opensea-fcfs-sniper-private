
pub async fn ping_api_with(client: &Client, api_key: &str) -> Result<()> {
    let resp = client
        .get(format!("{OPENSEA_API}/api/v2/drops?type=featured&limit=1"))
        .header("X-API-KEY", api_key)
        .header("Accept", "application/json")
        .send()
        .await
        .wrap_err("GET featured drops")?;
    let status = resp.status();
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        eyre::bail!("auth failed status={status}");
    }
    if !status.is_success() {
        eyre::bail!("status={status}");
    }
    Ok(())
}

async fn post_mint(
    client: &Client,
    api_key: &str,
    slug: &str,
    minter: Address,
    qty: u64,
) -> Result<(StatusCode, String)> {
    let url = format!("{OPENSEA_API}/api/v2/drops/{slug}/mint");
    let resp = client
        .post(&url)
        .header("X-API-KEY", api_key)
        .header("Content-Type", "application/json")
        .json(&json!({
            "minter": format!("{minter}"),
            "quantity": qty,
        }))
        .send()
        .await
        .wrap_err("POST mint")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    Ok((status, body))
}

fn parse_mint_tx(body: &str) -> Result<MintTx> {
    let v: Value = serde_json::from_str(body).wrap_err("mint json")?;
    let root = if v.get("to").is_some()
        || v.get("target").is_some()
        || v.get("data").is_some()
        || v.get("calldata").is_some()
    {
        &v
    } else if let Some(inner) = v.get("transaction").or_else(|| v.get("tx")) {
        inner
    } else {
        &v
    };

    let to_s = json_str(root, &["to", "target"])
        .ok_or_else(|| eyre!("missing to/target in mint response"))?;
    let data_s = json_str(root, &["data", "calldata"])
        .ok_or_else(|| eyre!("missing data/calldata in mint response"))?;
    let value_s = json_str(root, &["value"]).unwrap_or_else(|| "0".into());
    let chain = json_str(root, &["chain"]).or_else(|| json_str(&v, &["chain"]));

    let to = Address::from_str(to_s.trim()).wrap_err("mint to address")?;
    let data_bytes =
        hex::decode(data_s.trim().trim_start_matches("0x")).wrap_err("mint data hex")?;
    if data_bytes.is_empty() {
        eyre::bail!("mint data/calldata empty");
    }
    Ok(MintTx {
        to,
        data: Bytes::from(data_bytes),
        value: parse_u256(&value_s)?,
        chain,
    })
}

fn json_str(v: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(x) = v.get(*k) {
            match x {
                Value::String(s) if !s.is_empty() => return Some(s.clone()),
                Value::Number(n) => return Some(n.to_string()),
                _ => {}
            }
        }
    }
    None
}

fn parse_u256(s: &str) -> Result<U256> {
    let s = s.trim();
    if s.is_empty() || s == "0x" || s == "0X" {
        return Ok(U256::ZERO);
    }
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        if hex.is_empty() {
            return Ok(U256::ZERO);
        }
        return U256::from_str_radix(hex, 16).map_err(|e| eyre!("value hex: {e}"));
    }
    U256::from_str(s).map_err(|e| eyre!("value decimal: {e}"))
}

fn is_retryable(status: StatusCode, body: &str) -> bool {
    let code = status.as_u16();
    if code == 429 || status.is_server_error() {
        return true;
    }
    if code == 409 || code == 422 {
        return true;
    }
    let msg = error_message(body).to_lowercase();
    let hints = [
        "not currently active",
        "not started",
        "not open",
        "ineligible for the active mint stage",
        "no active stage",
        "paused",
        "coming soon",
        "stage is not",
        "mint has not started",
        "drop is not live",
        "not yet",
        "too early",
        "not eligible for the current",
    ];
    code >= 400 && code < 500 && hints.iter().any(|h| msg.contains(h))
}

fn error_message(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };
    if let Some(arr) = v.get("errors").and_then(|e| e.as_array()) {
        let parts: Vec<&str> = arr.iter().filter_map(|x| x.as_str()).collect();
        if !parts.is_empty() {
            return parts.join("; ");
        }
    }
    if let Some(s) = v.get("message").and_then(|m| m.as_str()) {
        return s.to_string();
    }
    body.to_string()
}

#[derive(Debug, Clone)]
pub struct StageStart {
    pub start_unix: i64,
    pub label: String,
    pub stage_type: String,
    pub source: &'static str,
}

/// Pick relevant upcoming/active stage start from GET /drops/{slug} JSON.
/// Prefers `active_stage`, then `next_stage`, then `stages[]` (active window, else soonest future).
pub fn pick_relevant_stage_start(details: &Value) -> Result<StageStart> {
    if let Some(st) = details.get("active_stage").filter(|v| v.is_object()) {
        if let Some(s) = stage_from_value(st, "active_stage") {
            return Ok(s);
        }
    }
    if let Some(st) = details.get("next_stage").filter(|v| v.is_object()) {
        if let Some(s) = stage_from_value(st, "next_stage") {
            return Ok(s);
        }
    }
    let stages = details
        .get("stages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let now = chrono::Utc::now().timestamp();
    let mut candidates: Vec<(StageStart, Option<i64>)> = Vec::new();
    for st in &stages {
        if let Some(s) = stage_from_value(st, "stages") {
            let end =
                json_str(st, &["end_time", "endTime"]).and_then(|e| parse_stage_time(&e).ok());
            candidates.push((s, end));
        }
    }
    if candidates.is_empty() {
        eyre::bail!("drop has no usable stage start_time / startTime");
    }
    for (s, end) in &candidates {
        if s.start_unix <= now && end.map(|e| now < e).unwrap_or(true) {
            return Ok(StageStart {
                source: "stages_active",
                ..s.clone()
            });
        }
    }
    let mut future: Vec<StageStart> = candidates
        .iter()
        .map(|(s, _)| s.clone())
        .filter(|s| s.start_unix > now)
        .collect();
    future.sort_by_key(|s| s.start_unix);
    if let Some(s) = future.into_iter().next() {
        return Ok(StageStart {
            source: "stages_upcoming",
            ..s
        });
    }
    candidates
        .into_iter()
        .map(|(s, _)| s)
        .max_by_key(|s| s.start_unix)
        .ok_or_else(|| eyre!("no stage start"))
}

fn stage_from_value(st: &Value, source: &'static str) -> Option<StageStart> {
    let start = json_str(st, &["start_time", "startTime"])?;
    let start_unix = parse_stage_time(&start).ok()?;
    let label = json_str(st, &["label", "name"]).unwrap_or_default();
    let stage_type = json_str(st, &["stage_type", "stageType", "type"]).unwrap_or_default();
    Some(StageStart {
        start_unix,
        label,
        stage_type,
        source,
    })
}

fn parse_stage_time(s: &str) -> Result<i64> {
    let t = s.trim();
    if t.is_empty() {
        eyre::bail!("empty stage time");
    }
    if let Ok(n) = t.parse::<i64>() {
        return Ok(if n.abs() >= 1_000_000_000_000 {
            n / 1000
        } else {
            n
        });
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(t) {
        return Ok(dt.timestamp());
    }
    let cleaned = t.trim_end_matches('Z').replace('T', " ");
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(&cleaned, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(&cleaned, "%Y-%m-%d %H:%M:%S%.f"))
    {
        use chrono::{TimeZone, Utc};
        return Ok(Utc.from_utc_datetime(&naive).timestamp());
    }
    eyre::bail!("unrecognized stage time: {t}")
}

fn truncate(s: &str, n: usize) -> &str {
    if s.len() <= n {
        s
    } else {
        &s[..n]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn picks_active_stage_first() {
        let details = json!({
            "active_stage": {
                "start_time": "1700000000",
                "label": "WL",
                "stage_type": "ALLOWLIST"
            },
            "next_stage": {
                "start_time": "1800000000",
                "label": "PUBLIC",
                "stage_type": "PUBLIC_SALE"
            }
        });
        let s = pick_relevant_stage_start(&details).unwrap();
        assert_eq!(s.start_unix, 1_700_000_000);
        assert_eq!(s.source, "active_stage");
    }

    #[test]
    fn picks_next_when_no_active() {
        let details = json!({
            "next_stage": {
                "startTime": "1800000000",
                "name": "Public",
                "type": "PUBLIC"
            }
        });
        let s = pick_relevant_stage_start(&details).unwrap();
        assert_eq!(s.start_unix, 1_800_000_000);
        assert_eq!(s.source, "next_stage");
    }

    #[test]
    fn picks_soonest_future_from_stages() {
        let now = chrono::Utc::now().timestamp();
        let details = json!({
            "stages": [
                {"start_time": now - 10_000, "end_time": now - 5_000, "label": "past"},
                {"start_time": now + 5_000, "label": "soon", "stage_type": "ALLOWLIST"},
                {"start_time": now + 9_000, "label": "later"}
            ]
        });
        let s = pick_relevant_stage_start(&details).unwrap();
        assert_eq!(s.label, "soon");
        assert!(s.source.contains("stages"));
    }

    #[test]
    fn parse_stage_time_secs_and_ms() {
        assert_eq!(parse_stage_time("1700000000").unwrap(), 1_700_000_000);
        assert_eq!(parse_stage_time("1700000000000").unwrap(), 1_700_000_000);
    }
}
