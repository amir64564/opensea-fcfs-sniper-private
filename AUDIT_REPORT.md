# OpenSea FCFS Sniper — Audit & Harden Report

**Date:** 2026-09-19 (IST)  
**Tree:** `/workspace/opensea-fcfs-sniper` (existing local; no git clone)  
**Verify:** `cargo fmt` + `cargo check` + `cargo test` → **16/16 passed**

## Problems found

1. **Telegram blocked during snipe** — long-poll awaited the full countdown/fire, so the bot was unresponsive while WAITING.
2. **No task lifecycle** — no WAITING→ARMED→FIRING→SUCCESS/FAILED/CANCELLED; easy to double-trigger from chat.
3. **Hot-path RPC rank** — `RPC_AUTO_RANK` could re-probe at fire time and add latency; a bad RPC probe could hurt the mint window.
4. **Weak error UX** — raw `eyre` chains to Telegram could be noisy and risk secret leakage.
5. **Thin success/fail notices** — missing structured mode/wallet-label/qty/tx/rpc/timing fields.
6. **No startup config validation** for telegram/panel beyond dotenv parse.
7. **No cancelable countdown** — Cancel Session could not abort an in-flight wait.
8. **No graceful shutdown** — SIGINT left background work undefined.
9. **Dead `fire_compact_test.rs`** — duplicate of `fire.rs`, not real tests; not wired into `main`.
10. **Timezone ambiguity** — go-time logs did not clearly show UTC vs IST.
11. **Security** — `.env` present locally (gitignored); no accidental secret commits in `src/` (only fake `0xaaa…` in unit test).

## Changes made

| Area | Change |
|------|--------|
| `task.rs` (new) | Lightweight lifecycle + single-flight gate + process-global cancel for countdown |
| `errclass.rs` (new) | Classifies config/rpc/opensea/tx/funds/timeout/soldout/already/cancelled/unknown; sanitizes secrets for Telegram |
| `timing.rs` | Cancelable sleep; UTC+IST formatting; explicit `--at` wins; unit tests |
| `fire.rs` | Skip re-rank when already prewarmed (WL hot path); per-RPC timeout; host labels; `FireOutcome`; bad inclusion probe ignored |
| `ops.rs` | Rank during wait window (not at fire); timezone logs; structured fire outcome log |
| `config.rs` | `validate_startup()` (no network) |
| `telegram.rs` | Background snipe jobs; responsive poll while WAITING; Cancel aborts; SIGINT shutdown; actionable errors; preserved commands |
| `opensea.rs` | Real stage-pick / time-parse unit tests |
| Removed | Dead `fire_compact_test.rs` duplicate |

## Files touched

`src/main.rs`, `src/task.rs`, `src/errclass.rs`, `src/timing.rs`, `src/fire.rs`, `src/ops.rs`, `src/config.rs`, `src/telegram.rs`, `src/opensea.rs` (+ deleted dead compact duplicate)

## Preserved behavior

- CLI: doctor / rank-rpc / bench / arm / fire / snipe / api-arm / api-snipe / panel / telegram  
- Telegram: `/password`, `/lock`, Snipe Setup, Arm, Cancel Session, `wl` / `public`, `/snipe_wl`, `/snipe_public`, `/rpc*`, `/rank`, `/rename_*`, `/import_wallet`, session API paste flow  
- Manual `--at` / explicit time wins over auto  
- `early_ms` preserved  
- Panel + RPC rank + password gate + session API flow  
- No new heavy deps; VPS-friendly  

## Tests (verified)

- `task`: single-flight, double-fire block, cancel-before-fire  
- `errclass`: classify + sanitize privkey  
- `timing`: normalize, explicit-at-wins, IST parse, cancelable sleep, zone labels  
- `fire`: truncate + outcome default  
- `opensea`: active/next/stages pick + ms/sec time parse  

## Remaining risks

1. Background Telegram jobs share process-global cancel — fine for single-operator VPS; not multi-tenant.  
2. Session wallet keys still held in memory for the job duration (required); wiped on SUCCESS/FAILED/CANCELLED.  
3. OpenSea API / RPC behavior still external — sold-out/already detection is heuristic from error text.  
4. Inclusion watch still optional; “SUCCESS” means broadcast accepted, not necessarily included unless `INCLUSION_WATCH_MS` > 0.  
5. Local `.env` must never be pushed (gitignored; excluded from GitHub sync).

## Claims

Only claiming what was run: **`cargo fmt`**, **`cargo check`**, **`cargo test` (16 passed)**.


## GitHub sync status (2026-09-19 IST)

Private repo: `amir64564/opensea-fcfs-sniper-private` (branch `main`).

### Synced (blob SHA match local)
- `src/task.rs`, `src/timing.rs`, `src/config.rs`, `src/errclass.rs`
- `src/logbuf.rs`, `src/seadrop.rs`, `src/ops.rs`, `src/arm.rs`, `src/main.rs`
- Docs/manifest already matched: `Cargo.toml`, `.gitignore`, `.env.example`, `README.md`, `AUDIT_REPORT.md`, `wallets.example.json`

### Removed from remote (obsolete)
- `src/fire_compact_test.rs`
- `src/telegram_part1.rs`, `src/telegram_part2a.rs`, `src/telegram_part2b.rs`

### Still need push (local newer / remote stale)
- `src/fire.rs` (~13.6KB)
- `src/opensea.rs` (~19KB)
- `src/panel.rs` (~18KB)
- `src/session.rs` (~26KB)
- `src/telegram.rs` (~42KB consolidated; remote still 319-byte stub)

**Do not push `.env` or secrets.** Local `cargo test` = 16/16; `cargo check` + `fmt` clean aside from unused-item warnings.
