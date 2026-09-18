# opensea-fcfs-sniper

Dual-mode **OpenSea WL + Public FCFS** sniper.

Daily use is **WL / signed / allowlist** (`api-snipe`). Pure on-chain public FCFS (`snipe`) is the faster fallback when the live stage is `mintPublicDrop` with no OpenSea signature.

Burner wallet + burner OpenSea API key are fine. Do **not** give friend-bots your `WALLET_KEY` or `OPENSEA_API_KEY`.

## Honest latency note
- **Public hot path:** single-digit ms from wake → raw broadcast *after* pre-sign (`DRY_RUN fire_local_hotpath_ms`).
- **WL hot path:** shared HTTP/2 client prewarmed (TLS+GET drop) before countdown → first OpenSea `200` with `to`/`data`/`value` → local EIP-1559 sign with cached nonce (no `estimateGas`, no slow dump before broadcast) → multi-RPC fan-out. API RTT dominates; this cannot beat a pre-signed public fire.
- **Not guaranteed:** NFT inclusion in N ms. Sequencer / preconfirm often 200–500ms+ on L2s.

## WL FCFS (primary — signed / allowlist)

Wallet is eligible; you still race big bots for supply. OpenSea must mint-sign at T-0 — you cannot precompute that calldata.

1. Prewarm OpenSea HTTP/2 + RPCs + nonce **before** countdown
2. At T-`early-ms`: hammer `POST /api/v2/drops/{slug}/mint` (default backoff 15ms, timeout 60s, `OPENSEA_HAMMER_PARALLEL=2`)
3. First `200` with `{to,data,value}` (aliases `target`/`calldata`) → sign EIP-1559 locally → `eth_sendRawTransaction` fan-out

`OPENSEA_API_KEY` is **required** for this path.

```bash
cp .env.example .env
# fill WALLET_KEY, RPC_URL, OPENSEA_API_KEY
cargo build --release
./target/release/opensea-fcfs-sniper doctor
./target/release/opensea-fcfs-sniper api-snipe --slug theroyalmechanica --qty 1 --at UNIX_SEC_OR_MS --early-ms 50 --yes
# dry-run:
./target/release/opensea-fcfs-sniper api-snipe --slug theroyalmechanica --qty 1 --at UNIX --early-ms 50 --dry-run
# arm now (if stage already returns calldata), fire later:
./target/release/opensea-fcfs-sniper api-arm --slug theroyalmechanica --qty 1 --out armed-api.json
./target/release/opensea-fcfs-sniper fire --armed armed-api.json --dry-run
```

**Outgas / RBF:** same nonce. Raise `PRIORITY_FEE_GWEI` + `MAX_FEE_GWEI`, re-run `api-arm` (or `api-snipe`) to re-sign, then `fire`. Optional `RBF_AFTER_MS` / `RBF_MAX` only wait-and-check inclusion — they do not bump fees by themselves.

## Public FCFS (secondary — on-chain SeaDrop)

No OpenSea API on the hot path. Pre-sign `mintPublicDrop` **before** T0; at T-`early-ms` only fan-out raw txs. This **is** faster than `api-snipe` when the stage is public on-chain. The API does **not** speed public FCFS.

```bash
./target/release/opensea-fcfs-sniper arm --nft 0xNFT --qty 1
./target/release/opensea-fcfs-sniper fire --armed armed.json --at UNIX --early-ms 50
./target/release/opensea-fcfs-sniper snipe --nft 0xNFT --qty 1 --at UNIX --early-ms 50 --yes
```

## Speed toolkit
```bash
./target/release/opensea-fcfs-sniper rank-rpc
./target/release/opensea-fcfs-sniper bench
./target/release/opensea-fcfs-sniper fire --armed armed.json --dry-run
```

See `PAID_RESEARCH.md` and `RESEARCH.md`.

## Control panel

Local UI for both modes. Binds **127.0.0.1 only**. Reads `.env`; shows wallet **address**, never the private key.

```bash
./target/release/opensea-fcfs-sniper panel
# open http://127.0.0.1:8787
```

Toggle **WL / signed** (default) or **Public**. Fill slug *or* nft, qty, go-time (unix or `YYYY-MM-DD HH:MM:SS` IST), early-ms, dry-run / yes. Buttons call Doctor / Arm / Fire / Snipe in-process (same code as the CLI). Live log stream is on the right.


## Telegram control

Remote control via long-poll bot (`telegram` subcommand). Only `TELEGRAM_CHAT_ID` may send commands. **Never logs private keys or full OpenSea API keys** (masked `first4…last4`).

### Setup (@BotFather)
1. Open Telegram → talk to [@BotFather](https://t.me/BotFather)
2. `/newbot` → pick name + username → copy the **bot token**
3. Start a chat with your bot, send `/start`
4. Get your chat id (e.g. message [@userinfobot](https://t.me/userinfobot) or check `getUpdates`)
5. Put into `.env`:
```bash
TELEGRAM_BOT_TOKEN=<token from BotFather>
TELEGRAM_CHAT_ID=123456789
```
6. Run:
```bash
./target/release/opensea-fcfs-sniper telegram
```

### Snipe Setup flow (session OpenSea API keys)
1. Tap **Snipe Setup** → pick wallet(s) by number (`1`, `1,2`, or `all`) from `WALLET_KEY` / `wallets.json`
2. Bot shows the selected wallet and asks: *Send the OpenSea API key for this wallet.*
3. Paste a **NEW** API key (session-only; not a permanent `API_1` vault). Optionally set an API display name (`/skip` to skip).
4. Multi-wallet: repeats key (and optional name) for each selected wallet in order — strict wallet→key map for this session.
5. Tap **Arm**, then send `wl <slug> <qty> <at> [early_ms] [dry]` or `public <nft> …` (or `/snipe_wl` / `/snipe_public`).
6. On SUCCESS / FAILED / TIMEOUT / **Cancel Session**: temporary OpenSea API key material is wiped (memory + `/tmp` 0600 file). Wallet keys + wallet display names **persist**.

### Wallet import & names
- `/import_wallet <pk>` → then *Set your wallet name* (saved to `wallet_names.json` / `wallets.json` label)
- `/rename_wallet <index|label|address> <name>`
- `/rename_api <selected_index|label|address> <name>` — session UI label only (secret still wiped at session end)

### Other commands
- `/doctor` `/status` `/session` `/wallets` `/help` `/panel_hint`
- `/rpc` `/rpc_set` `/rpc_add` `/rpc_clear_extra` `/rank`

Append `dry` to skip broadcast. Live snipes spend gas from the selected wallet on the host running the bot.
