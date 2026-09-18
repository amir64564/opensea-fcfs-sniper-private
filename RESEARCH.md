# OpenSea FCFS Sniper — Research Brief (2026-09-12)

## Why tonight failed (theroyalmechanica / OSNM-Z)
1. **Stage 3 SIGNED_PRESALE @ T-0:** OpenSea returned `wallet is ineligible for the active mint stage` then retries hit `exceeds an allocation or supply limit`.
2. That path is **server-signed / WL** — needs OpenSea signature API. Not a pure on-chain FCFS race.
3. OSNM-Z hot path still does interactive setup, OpenSea calldata fetches, funding gates, single-RPC style submission → too much work near T-0.
4. **Public FCFS** (Stage 0 @ 16:00 UTC) is the real speed fight — must be on-chain SeaDrop `mintPublicDrop` with pre-signed raw tx.

## What elite snipers actually do
| Layer | Technique | Why |
|---|---|---|
| Cold | Resolve slug → NFT + SeaDrop addresses; read `getPublicDrop` on-chain | Avoid OpenSea API on hot path |
| Arm | Pre-sign EIP-1559 raw tx; fixed gas; nonce cache; HTTP keep-alive; RPC rank | At T-0 only `eth_sendRawTransaction` |
| Fire | Multi-RPC fan-out (first wins); optional early-ms; RBF bumps | Win sequencer race / stuck-tx recovery |
| Signed WL | Separate path: hammer OpenSea `/mint` for signature then broadcast | Cannot precompute OpenSea EIP-712 alone |

References studied: dhasap/nft-mint-agent, morsyxbt/nft-public-mint, OpenSea SeaDrop.sol, Base Flashblocks latency notes.

## 9 millisecond honesty
- **Achievable:** local hot-path after pre-sign (serialize already done → write syscall + RPC POST start) in low single-digit ms on a warm connection.
- **Not a promise:** full inclusion. Premium L2 RPC *reads* can be ~8–15ms; sequencer preconfirm often **200–500ms+**. Marketing “9ms mint” usually means client fire latency, not NFT in wallet.
- Design target: **minimize client fire latency + maximize broadcast fan-out**, measure `arm→send` p50/p99.

## Build target for 08:00 IST
Rust CLI `opensea-fcfs-sniper`: doctor / arm / fire / dry-run, public SeaDrop first, OpenSea-signed secondary stub, multi-RPC + RBF + timing metrics.
