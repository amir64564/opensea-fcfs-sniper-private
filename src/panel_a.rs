use crate::arm;
use crate::config::AppConfig;
use crate::fire;
use crate::logbuf;
use crate::ops;
use crate::outln;
use crate::timing;
use eyre::Result;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

static BUSY: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Deserialize)]
struct JobReq {
    #[serde(default)]
    mode: String,
    #[serde(default)]
    slug: String,
    #[serde(default)]
    nft: String,
    #[serde(default = "one")]
    qty: u64,
    #[serde(default)]
    at: String,
    #[serde(default = "early_def")]
    early_ms: i64,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    yes: bool,
}

fn one() -> u64 {
    1
}
fn early_def() -> i64 {
    50
}

fn is_wl(mode: &str) -> bool {
    let m = mode.trim().to_ascii_lowercase();
    m == "wl" || m == "api" || m == "signed" || m == "allowlist"
}

pub async fn serve(bind: &str) -> Result<()> {
    if !bind.starts_with("127.0.0.1") && !bind.starts_with("localhost") {
        eyre::bail!("panel binds loopback only (got {bind})");
    }
    let listener = TcpListener::bind(bind).await?;
    outln!("panel listening http://{bind}  (local only — wallet key never sent to UI)");
    let bind = bind.to_string();
    loop {
        let (stream, addr) = listener.accept().await?;
        if !addr.ip().is_loopback() {
            drop(stream);
            continue;
        }
        let bind = bind.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, &bind).await {
                eprintln!("panel conn: {e}");
            }
        });
    }
}

async fn handle(mut stream: TcpStream, bind: &str) -> Result<()> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 64 * 1024 {
            return respond(&mut stream, 413, "text/plain", b"too large").await;
        }
    }
    let header_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(buf.len());
    let header = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = header.split("\r\n");
    let req = lines.next().unwrap_or("");
    let mut parts = req.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let path_q = parts.next().unwrap_or("/").to_string();
    let (path, query) = match path_q.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (path_q, String::new()),
    };
    let mut content_len: usize = 0;
    for line in lines {
        let l = line.to_ascii_lowercase();
        if let Some(v) = l.strip_prefix("content-length:") {
            content_len = v.trim().parse().unwrap_or(0);
        }
    }
    let body_start = header_end + 4;
    while buf.len() < body_start + content_len {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 256 * 1024 {
            return respond(&mut stream, 413, "text/plain", b"too large").await;
        }
    }
    let body = buf.get(body_start..body_start + content_len).unwrap_or(&[]);
    route(&mut stream, &method, &path, &query, body, bind).await
}

async fn route(
    stream: &mut TcpStream,
    method: &str,
    path: &str,
    query: &str,
    body: &[u8],
    _bind: &str,
) -> Result<()> {
    match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => {
            respond(
                stream,
                200,
                "text/html; charset=utf-8",
                PANEL_HTML.as_bytes(),
            )
            .await
        }
        ("GET", "/api/status") => json_ok(stream, status_json()).await,
        ("GET", "/api/logs") => {
            let from = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("from="))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let (lines, total) = logbuf::snapshot(from);
            json_ok(
                stream,
                json!({"lines": lines, "total": total, "busy": BUSY.load(Ordering::Relaxed)}),
            )
            .await
        }
        ("POST", "/api/logs/clear") => {
            logbuf::clear();
            json_ok(stream, json!({"ok": true})).await
        }
        ("POST", "/api/doctor") => {
            spawn_job(stream, "doctor", async {
                outln!("— doctor —");
                config_doctor().await
            })
            .await
        }
        ("POST", "/api/arm") => {
            let req: JobReq = serde_json::from_slice(body).unwrap_or(empty_req());
            spawn_job(stream, "arm", async move { run_arm(req).await }).await
        }
        ("POST", "/api/fire") => {
            let req: JobReq = serde_json::from_slice(body).unwrap_or(empty_req());
            spawn_job(stream, "fire", async move { run_fire(req).await }).await
        }
        ("POST", "/api/snipe") => {
            let req: JobReq = serde_json::from_slice(body).unwrap_or(empty_req());
            spawn_job(stream, "snipe", async move { run_snipe(req).await }).await
        }
        _ => respond(stream, 404, "text/plain", b"not found").await,
    }
}

fn empty_req() -> JobReq {
    JobReq {
        mode: "wl".into(),
        slug: String::new(),
        nft: String::new(),
        qty: 1,
        at: String::new(),
        early_ms: 50,
        dry_run: true,
        yes: false,
    }
}

fn status_json() -> Value {
    match AppConfig::from_env() {
        Ok(cfg) => json!({
            "ok": true,
            "wallet": format!("{}", cfg.wallet.address()),
            "chain_id": cfg.chain_id,
            "rpcs": cfg.rpc_urls.len(),
            "seadrop": format!("{}", cfg.seadrop),
            "gas_limit": cfg.gas_limit,
            "max_fee_gwei": cfg.max_fee_gwei,
            "priority_fee_gwei": cfg.priority_fee_gwei,
            "has_api_key": cfg.opensea_api_key.as_ref().map(|k| !k.is_empty()).unwrap_or(false),
            "wl_path": "opensea_api_calldata",
            "busy": BUSY.load(Ordering::Relaxed),
        }),
        Err(e) => {
            json!({"ok": false, "error": format!("{e}"), "busy": BUSY.load(Ordering::Relaxed)})
        }
    }
}

async fn config_doctor() -> Result<()> {
    crate::config::doctor().await
}

async fn run_arm(req: JobReq) -> Result<()> {
    if is_wl(&req.mode) {
        if req.slug.trim().is_empty() {
            eyre::bail!("WL arm needs collection slug");
        }
        outln!("— api-arm slug={} qty={} —", req.slug, req.qty);
        arm::arm_api(req.slug.trim(), req.qty, "armed-api.json").await
    } else {
        if req.nft.trim().is_empty() {
            eyre::bail!("Public arm needs nft address");
        }
        outln!("— arm nft={} qty={} —", req.nft, req.qty);
        arm::arm_public(req.nft.trim(), req.qty, "armed.json").await
    }
}

async fn run_fire(req: JobReq) -> Result<()> {
    let armed = if is_wl(&req.mode) {
        "armed-api.json"
    } else {
        "armed.json"
    };
    let at = if req.at.trim().is_empty() {
        None
    } else {
        Some(timing::parse_go_time(req.at.trim())?)
    };
    outln!(
        "— fire armed={armed} dry_run={} early_ms={} at={:?} —",
        req.dry_run,
        req.early_ms,
        at
    );
    fire::fire_armed(armed, req.dry_run, req.early_ms, at).await
}

async fn run_snipe(req: JobReq) -> Result<()> {
    if !req.dry_run && !req.yes {
        eyre::bail!("refusing live snipe without yes=true (or set dry_run)");
    }
    let at = timing::resolve_at_arg(
        if req.at.trim().is_empty() {
            None
        } else {
            Some(req.at.trim())
        },
        false,
    )?;
    if is_wl(&req.mode) {
        if req.slug.trim().is_empty() {
            eyre::bail!("WL snipe needs collection slug");
        }
        outln!(
            "— api-snipe (OpenSea calldata hotpath) slug={} qty={} at={:?} early_ms={} dry_run={} —",
            req.slug,
            req.qty,
            at,
            req.early_ms,
            req.dry_run
        );
        ops::run_api_snipe(req.slug.trim(), req.qty, at, req.early_ms, req.dry_run).await
    } else {
        if req.nft.trim().is_empty() {
            eyre::bail!("Public snipe needs nft address");
        }
        outln!(
            "— snipe nft={} qty={} at={:?} early_ms={} dry_run={} —",
            req.nft,
            req.qty,
            at,
            req.early_ms,
            req.dry_run
        );
        ops::run_public_snipe(req.nft.trim(), req.qty, at, req.early_ms, req.dry_run).await
    }
}
