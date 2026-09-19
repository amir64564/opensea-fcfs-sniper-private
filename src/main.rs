mod arm;
mod config;
mod errclass;
mod fire;
mod logbuf;
mod opensea;
mod ops;
mod panel;
mod seadrop;
mod session;
mod task;
mod telegram;
mod timing;

use clap::{Parser, Subcommand};
use eyre::Result;

use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "opensea-fcfs-sniper",
    about = "Dual-mode OpenSea WL+Public FCFS sniper"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    Doctor,
    /// Rank RPCs by latency
    RankRpc,
    /// Measure local hotpath decode latency (no network)
    Bench {
        #[arg(long, default_value_t = 10000)]
        iters: u32,
    },
    /// Public FCFS: pre-sign on-chain mintPublicDrop
    Arm {
        #[arg(long)]
        nft: String,
        #[arg(long, default_value_t = 1)]
        qty: u64,
        #[arg(long, default_value = "armed.json")]
        out: String,
    },
    Fire {
        #[arg(long, default_value = "armed.json")]
        armed: String,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        #[arg(long, default_value_t = 0)]
        early_ms: i64,
        #[arg(long)]
        at: Option<i64>,
    },
    /// Public FCFS: arm then fire at T-early (no OpenSea API on hot path)
    Snipe {
        #[arg(long)]
        nft: String,
        #[arg(long, default_value_t = 1)]
        qty: u64,
        /// Unix / IST datetime, or `auto`. Omit + `--auto-time` = on-chain getPublicDrop startTime.
        #[arg(long)]
        at: Option<String>,
        #[arg(long, default_value_t = false)]
        auto_time: bool,
        #[arg(long, default_value_t = 50)]
        early_ms: i64,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// WL FCFS: hammer OpenSea mint API, sign locally, write armed packet
    ApiArm {
        #[arg(long)]
        slug: String,
        #[arg(long, default_value_t = 1)]
        qty: u64,
        #[arg(long, default_value = "armed-api.json")]
        out: String,
    },
    /// WL FCFS (primary): prewarm, hammer at T-early, sign, multi-RPC fire
    ApiSnipe {
        #[arg(long)]
        slug: String,
        #[arg(long, default_value_t = 1)]
        qty: u64,
        /// Unix / IST datetime, or `auto`. Omit / `--auto-time` = OpenSea drop stage startTime.
        #[arg(long)]
        at: Option<String>,
        #[arg(long, default_value_t = false)]
        auto_time: bool,
        #[arg(long, default_value_t = 50)]
        early_ms: i64,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// Local 127.0.0.1 control panel (WL + Public)
    Panel {
        #[arg(long, default_value = "127.0.0.1:8787")]
        bind: String,
    },
    /// Telegram long-poll control (TELEGRAM_BOT_TOKEN + TELEGRAM_BOT_PASSWORD)
    Telegram,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Doctor => config::doctor().await?,
        Cmd::RankRpc => config::rank_rpc().await?,
        Cmd::Bench { iters } => {
            let t0 = std::time::Instant::now();
            let sample = "02f8"; // tiny
            for _ in 0..iters {
                let _ = hex::decode(sample);
            }
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            println!(
                "bench_hex_decode iters={iters} total_ms={ms:.4} per_iter_us={:.4}",
                (ms * 1000.0) / iters as f64
            );
            // Local EIP-1559 sign+encode (no network) — WL hotpath after OpenSea calldata lands.
            match config::AppConfig::from_env() {
                Ok(cfg) => {
                    use alloy::primitives::{Bytes, U256};
                    let to = cfg.seadrop;
                    let data = Bytes::from(vec![0x12u8; 196]); // typical mint calldata size-ish
                    let value = U256::from(0u64);
                    let mut times_us = Vec::with_capacity(iters as usize);
                    // warmup
                    for _ in 0..32 {
                        let _ = arm::sign_call(
                            &cfg,
                            to,
                            data.clone(),
                            value,
                            value,
                            "bench".into(),
                            1,
                            0,
                            0,
                            0,
                        )?;
                    }
                    for i in 0..iters {
                        let t = std::time::Instant::now();
                        let _ = arm::sign_call(
                            &cfg,
                            to,
                            data.clone(),
                            value,
                            value,
                            "bench".into(),
                            1,
                            0,
                            0,
                            i as u64,
                        )?;
                        times_us.push(t.elapsed().as_secs_f64() * 1_000_000.0);
                    }
                    times_us.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    let p50 = times_us[times_us.len() / 2];
                    let p99 = times_us[((times_us.len() as f64) * 0.99) as usize];
                    let mean = times_us.iter().sum::<f64>() / times_us.len() as f64;
                    println!(
                        "bench_sign_encode iters={iters} mean_us={mean:.2} p50_us={p50:.2} p99_us={p99:.2} min_us={:.2} max_us={:.2}",
                        times_us[0], times_us[times_us.len() - 1]
                    );
                }
                Err(e) => println!("bench_sign_encode skipped (config: {e})"),
            }
            if std::path::Path::new("armed.json").exists() {
                fire::fire_armed("armed.json", true, 0, None).await?;
            }
        }
        Cmd::Arm { nft, qty, out } => arm::arm_public(&nft, qty, &out).await?,
        Cmd::Fire {
            armed,
            dry_run,
            early_ms,
            at,
        } => fire::fire_armed(&armed, dry_run, early_ms, at).await?,
        Cmd::Snipe {
            nft,
            qty,
            at,
            auto_time,
            early_ms,
            dry_run,
            yes,
        } => {
            if !yes && !dry_run {
                eyre::bail!("refusing live snipe without --yes (or pass --dry-run)");
            }
            let at = timing::resolve_at_arg(at.as_deref(), auto_time)?;
            ops::run_public_snipe(&nft, qty, at, early_ms, dry_run).await?;
        }
        Cmd::ApiArm { slug, qty, out } => arm::arm_api(&slug, qty, &out).await?,
        Cmd::ApiSnipe {
            slug,
            qty,
            at,
            auto_time,
            early_ms,
            dry_run,
            yes,
        } => {
            if !yes && !dry_run {
                eyre::bail!("refusing live api-snipe without --yes (or pass --dry-run)");
            }
            let at = timing::resolve_at_arg(at.as_deref(), auto_time)?;
            ops::run_api_snipe(&slug, qty, at, early_ms, dry_run).await?;
        }
        Cmd::Panel { bind } => panel::serve(&bind).await?,
        Cmd::Telegram => telegram::run().await?,
    }
    Ok(())
}
