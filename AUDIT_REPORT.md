# OpenSea FCFS Sniper — Audit & Harden Report

Date: 2026-09-19 (IST / Asia/Kolkata)
Local tree: `/workspace/opensea-fcfs-sniper` (no git clone; existing codebase)
Verified: `cargo fmt` (clean aside from unused warnings), `cargo check`, `cargo test` — **16/16 passed**

## Problems found
1. Telegram split across stub + `telegram_part*` modules — hard to maintain; remote still had obsolete parts.
2. No explicit snipe lifecycle / single-flight guard — risk of duplicate execution.
3. Countdown not cancelable from Telegram Cancel / SIGINT mid-wait.
4. Auto-time vs explicit `--at` needed clear precedence; timezone labels (UTC+IST) unclear in logs.
5. RPC re-rank on fire hot path added latency; bad RPC could stall mint.
6. Errors to Telegram not classified; risk of leaking secrets in messages.
7. Success/fail notifications incomplete (wallet label, RPC host, timing).
8. No lightweight startup config validation (no network).
9. Telegram poll blocked while snipe job ran (unresponsive bot).
10. Dead `fire_compact_test.rs` / fragmented telegram parts on remote.

## Changes made (local — verified by tests)
1. **`src/task.rs`** — WAITING→ARMED→FIRING→SUCCESS|FAILED|CANCELLED + `TaskGate` single-flight + process-global cancel flag.
2. **`src/errclass.rs`** — classify errors (config/rpc/opensea/tx/funds/timeout/soldout/already/cancelled/unknown) + sanitize secrets for Telegram.
3. **`src/timing.rs`** — cancelable countdown; explicit `--at` wins over auto; UTC+IST formatting; unit tests.
4. **`src/fire.rs`** — skip re-rank when prewarmed; per-RPC timeout; host labels; `FireOutcome`; bad inclusion probe ignored.
5. **`src/ops.rs`** — rank RPCs during wait window (not at fire); map outcomes; timezone logs.
6. **`src/config.rs`** — `validate_startup()` (no network); RPC host labels without full URLs/keys.
7. **`src/telegram.rs`** — consolidated; background jobs via `tokio::spawn` + mpsc; Cancel aborts wait; actionable errors; commands preserved (`/password`, `/lock`, Snipe Setup, wl/public, rpc_*, rename_*).
8. **`src/opensea.rs`** — stage-pick / time-parse unit tests.
9. Security scan — no real secrets in `src/`; only fake `0xaaa…` in unit test.
10. Concise diagnostic logs; graceful cancel without DB.

## Files touched (local)
`src/task.rs`, `src/errclass.rs`, `src/timing.rs`, `src/config.rs`, `src/ops.rs`, `src/main.rs`, `src/fire.rs`, `src/opensea.rs`, `src/arm.rs`, `src/seadrop.rs`, `src/logbuf.rs`, `src/session.rs`, `src/panel.rs`, `src/telegram.rs`, plus docs/`AUDIT_REPORT.md`.

## Preserved behavior
CLI, Telegram commands, WL/Public modes, panel, RPC rank, password gate, auto-time, session API flow, VPS-friendly (no new deps).

## Tests
- Unit: task single-flight / double-fire / cancel; errclass classify+sanitize; timing normalize/explicit-at/IST/cancelable sleep; opensea stage/time helpers; fire truncate/outcome defaults.
- `cargo test`: **16 passed**.

## Remaining risks
- Process-global cancel is single-operator (one live snipe).
- Keys remain in memory for job lifetime.
- Soldout/already classification is heuristic.
- SUCCESS = broadcast accepted unless inclusion watch is set.
- Never commit `.env`.

## GitHub sync status (2026-09-19 IST)
Private repo: `amir64564/opensea-fcfs-sniper-private` (`main`).

### Synced (blob SHA match local)
`src/task.rs`, `src/timing.rs`, `src/config.rs`, `src/errclass.rs`, `src/logbuf.rs`, `src/seadrop.rs`, `src/ops.rs`, `src/arm.rs`, `src/main.rs`
Docs/manifest already matched: `Cargo.toml`, `.gitignore`, `.env.example`, `README.md`, `wallets.example.json`

### Removed from remote (obsolete)
`src/fire_compact_test.rs`, `src/telegram_part1.rs`, `src/telegram_part2a.rs`, `src/telegram_part2b.rs`

### Still need push (local newer / remote stale)
- `src/fire.rs` (~13.6KB)
- `src/opensea.rs` (~19KB)
- `src/panel.rs` (~18KB)
- `src/session.rs` (~26KB)
- `src/telegram.rs` (~42KB consolidated; remote still ~319-byte stub)

**Do not push `.env` or secrets.** Parent/follow-up should finish remaining `push_files` batches from `/workspace/opensea-fcfs-sniper` using user-Github MCP with exact local bytes.
