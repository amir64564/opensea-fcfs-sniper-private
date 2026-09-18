# SPEED

Honest numbers from this box (2026-09-18 IST). Local hotpath is done; **RPC path dominates**.

## Local (already fast — not the bottleneck)

| Metric | Measured |
|---|---|
| `DRY_RUN fire_local_hotpath_ms` | **0.003–0.006 ms** |
| EIP-1559 sign+encode p50 | **~60–100 µs** |

## Network / chain (what you must optimize)

| Metric | Typical on public Alchemy |
|---|---|
| doctor / `eth_blockNumber` (public Alchemy here) | **~274–301 ms** measured |
| `fire_broadcast_ms` (first `eth_sendRawTransaction` HTTP result) | **~200–300 ms** |
| `inclusion_ms` (receipt non-null) | often **200–500 ms+** (sequencer) |

Sub-ms local fire does **not** buy sub-100 ms inclusion. Buy private RPCs + tune `early_ms`.

## Plug private / premium RPCs

```bash
# .env — private PRIMARY, extras for fan-out, public only as last resort
RPC_URL=https://YOUR-PRIVATE-RPC          # Blast / QuikNode / chain-native / self-hosted
BROADCAST_RPCS=https://priv-b/...,https://priv-c/...,https://public-alchemy-fallback/...
RPC_AUTO_RANK=1                           # default: re-rank by eth_blockNumber before fire (parallel)
INCLUSION_WATCH_MS=3000                   # optional: print inclusion_ms
```

Then:

```bash
./target/release/opensea-fcfs-sniper rank-rpc   # print p50 order
./target/release/opensea-fcfs-sniper doctor
```

On live `fire` / `api-snipe`, with `RPC_AUTO_RANK=1`, the binary:
1. Probes all URLs with parallel `eth_blockNumber`
2. Reorders fastest-first
3. Still broadcasts with **parallel** `join_all` (never sequential)
4. Prints `fire_broadcast_ms` (first success), per-rpc `broadcast_ms=…`, and `inclusion_ms` if watching

WL path also prints `opensea_mint_ms=…` on its own line (API RTT, separate from RPC fire).

## Manual `early_ms` tune

Fire at `T - early_ms`. Too early → tx may land before stage opens / get dropped. Too late → lose FCFS.

1. Use `rank-rpc` + private RPCs so `fire_broadcast_ms` is stable
2. Start `early_ms=50`, dry-run timing against go-live clock skew
3. Adjust ±10–25 ms from observed sequencer delay — **manual**, drop-specific

## Rebuild

```bash
RUSTFLAGS='-C target-cpu=native' cargo build --release
./target/release/opensea-fcfs-sniper rank-rpc
./target/release/opensea-fcfs-sniper fire --armed armed.json --dry-run
```
