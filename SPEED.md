# SPEED

Honest numbers from this box (2026-09-18 09:39 IST), `cargo build --release` with `RUSTFLAGS='-C target-cpu=native'`.

## What is fast (local)

| Path | Metric | Measured |
|---|---|---|
| Public dry-run fire | `DRY_RUN fire_local_hotpath_ms` (hex decode of pre-signed raw tx) | **0.0034–0.0055 ms** |
| Bench hex decode | per-iter (1000) | ~0.02–0.03 µs |
| Bench EIP-1559 sign+encode | mean / p50 / p99 (1000 iters, warmup 32) | **~64–105 µs / ~62–99 µs / ~94–154 µs** |

Public hot path after `arm`: wake → multi-RPC `eth_sendRawTransaction` only. Local work is **well under 0.1 s** (typically microseconds). Sign+encode for WL after OpenSea calldata is ~0.1 ms.

## What is slow (network / chain)

| Path | Measured / typical |
|---|---|
| RPC doctor latency (this box → configured RPC) | **~308 ms** |
| `eth_sendRawTransaction` RTT | often **~200–300 ms** |
| L2 sequencer / inclusion | often **200–500 ms+** — not controlled here |

**Inclusion is RPC- and sequencer-bound.** Sub-ms local fire does **not** guarantee sub-100 ms confirmation.

## Architecture speed wins

**Public:** pre-sign → hotpath fan-out only; `tcp_nodelay`; HTTP/2 keep-alive on RPC client; prewarm; `early_ms`.

**WL:** shared OpenSea HTTP/2 client; parallel mint hammer; sparse logs; cached nonce from prewarm; sign on first `200`; no `estimateGas`.

## Rebuild

```bash
RUSTFLAGS='-C target-cpu=native' cargo build --release
./target/release/opensea-fcfs-sniper doctor
./target/release/opensea-fcfs-sniper bench
./target/release/opensea-fcfs-sniper fire --armed armed.json --dry-run
```
