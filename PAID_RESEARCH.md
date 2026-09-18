# Paid NFT bot landscape (overnight scan)

| Product | Model | Relevant speed features to copy |
|---|---|---|
| Quasarr | Desktop non-custodial | Multi-wallet groups, dry-run/sim before fire, EIP-1559 ceilings, OpenSea + raw calldata |
| Moonpad | ~0.1 ETH/mo | Private node path, out-gas pending, multi-task, marketplace snipes |
| NFT Sensei | 0.1–0.5 ETH/mo | Mempool/frontrun modules, dynamic gas, sig-mint dashboard, multi-wallet |
| Open-source bar | free | Pre-sign raw tx, multi-RPC fan-out, RBF, early-ms, RPC rank (nft-mint-agent / seadrop-noir) |

## Our v0.2 response
We cannot buy Moonpad private nodes without Rudra's payment — instead we implement the **same architectural edges that actually matter for OpenSea SeaDrop FCFS**:
1. On-chain public drop arm (no OpenSea API on hot path)
2. Multi-RPC fan-out + keep-alive prewarm
3. RBF bump loop
4. Tip ladder (multiple pre-signed tip levels)
5. RPC latency ranking
6. Dry-run + measured hotpath ms
