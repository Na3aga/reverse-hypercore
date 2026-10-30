# HyperCore + HyperEVM MVP (clean-room)

A local, deterministic version of the Hyperliquid flow:
- **Chain:** 4 validators running Commonware simplex BFT over authenticated TCP. About 10 blocks/s, with ~130–180 ms from browser order to finalized and executed.
  - `hc-devnet` runs every validator in one process; `hc-validator` runs **one validator per process**, each with its own mempool, executor, state, block log and API. Transactions gossip between them over the same authenticated p2p network, and a node that was down downloads the blocks it missed from its peers (see [One validator per process](#one-validator-per-process)).
- **HyperCore DEX:**
  - perpetuals on a central limit order book (BTC, ETH) with USDC collateral;
  - limit, market and cancel orders, post-only and IOC;
  - TP/SL trigger orders, using Hyperliquid's wire format (`{"trigger":{"isMarket","triggerPx","tpsl"}}`):
    - they fire when the mark price crosses the trigger, and execute as a market order (mark ± 10%) or a limit order;
    - a TP/SL sent in the same action as an entry order (`"grouping":"normalTpsl"`) waits for that entry to fill;
    - TP/SL can cover part of a position, so a position can scale out at several prices;
    - when the position closes, the remaining TP/SL is cancelled;
  - **TWAP orders** (`twapOrder`, `twapCancel`): a market slice every 30 s within 3% of the mark, catching up after thin liquidity, optionally randomized;
  - per-account risk limits (`setRiskLimits`), enforced by the engine: max position (USD), max leverage, a daily loss limit that blocks new exposure until 00:00 UTC, and auto-close at a maintenance-margin percentage;
  - **cross and isolated margin** with tiered maintenance margin. An isolated position holds its own margin (`updateLeverage {isCross: false}`, `updateIsolatedMargin`), so its losses stop there and it is liquidated on its own;
  - hourly funding (formula checked against 1,500 real testnet records);
  - partial and backstop liquidation, and **auto-deleveraging (ADL)**: an account that falls below zero is closed at its bankruptcy price against the most profitable, most leveraged opposite positions, so no bad debt is left;
  - a stake-weighted median oracle fed with **live spot prices** from eight exchanges, weighted as on Hyperliquid.
- **HyperEVM:**
  - a revm-based EVM inside the same blocks and the same state hash;
  - **small and big EVM blocks** on one chain: a small block (2M gas) every second and a big one (30M gas) every minute. An account picks big blocks with `evmUserModify {usingBigBlocks: true}`;
  - read precompiles at `0x…0800`, `0x…0806`, `0x…0807`, `0x…080F` and `0x…0810` for positions, mark, oracle, account value and core balance;
  - a **CoreWriter** at `0x3333…3333`, so contracts can place and cancel core orders. Orders from CoreWriter run on core 3 s after their EVM block, so contracts get no latency edge;
  - a **bridge**: the `sendToEvm` action moves USDC from core to EVM, and sending value to `0x2222…2222` moves it back.
- **One key, one address:** secp256k1 keys sign both core actions and EVM transactions, so a trader's core account and EVM account share an address.
- **Hyperliquid's signing:** a core action is signed as EIP-712 typed data `Agent{source, connectionId}`, where `connectionId = keccak256(msgpack(action) ‖ nonce ‖ vault byte)`. A Hyperliquid client library produces valid signatures for this node: `scripts/check-js-signer.mjs` checks the Rust signer, the browser signer and the independent `@nktkas/hyperliquid` SDK against each other.
- **Agent (API) wallets:** an account approves a key with the user-signed `approveAgent` (EIP-712 in the `HyperliquidSignTransaction` domain, on whatever chain the wallet is on), and that key's actions then act for the account. The trading UI's **MetaMask…** wallet works this way, as on Hyperliquid: one wallet signature, then orders without popups.
- **Vaults:** `createVault` opens an account that its leader trades for followers; `vaultTransfer` deposits (shares at the current value per share) and withdraws (after a 1-day lockup; the leader gets 10% of a follower's profit and keeps at least 5% of the vault). The leader trades it by signing with `vaultAddress`, which is part of the signed hash, as is `expiresAfter` (rejected once the block is later). In the UI: the Vaults tab, and "Trade as" next to the address.
- **Transfers:** `usdSend` moves USDC between core accounts. It is user-signed like `approveAgent` (`HyperliquidTransaction:UsdSend`), so an agent can never move funds; the UI's Send USDC asks the browser wallet each time.
- **Interfaces:**
  - a trading terminal at `/` and a block explorer at `/explorer`. The terminal has:
    - a candlestick chart with your fills as B/S markers, plus your entry price and resting orders as price lines;
    - the order book, trades and order entry, with a Mid button that sets the price from the book;
    - TP/SL on new orders and on open positions (whole or part), drawn on the chart, and a Risk limits tab;
    - Limit, Market and TWAP order types, a TWAP tab with progress, cross/isolated margin with add/remove margin, liquidation prices, and a big-EVM-blocks switch;
    - a notification bell that groups fills, so only your own actions show toasts;
  - Hyperliquid-style WebSocket and `/info`/`/exchange` APIs, including `candleSnapshot` and the `candle` subscription;
  - Ethereum JSON-RPC at `/evm` for Foundry and viem.
- **Simulated market:** by default, background market makers and takers trade about 100 times per block (around 800–1,000 trades/s). They send ordinary signed transactions, so all of it is in the block log and replays.
- **Verifiability:**
  - a block log that replays to identical state hashes;
  - a supply check that holds to the µUSDC: core balances + isolated margin − position cost basis + USDC on the EVM = USDC minted.

Behaviour comes from public documentation, black-box measurements, and type and string metadata in `hl-node`, as listed in `../docs/FINDINGS.md`. No decompiled code was used.

## Quick start

```sh
cargo run --release --bin hc-devnet          # all 4 validators + API on http://127.0.0.1:8080
open http://127.0.0.1:8080                   # trading terminal (pick alice/bob/carol/dave)
open http://127.0.0.1:8080/explorer          # explorer: blocks, txs, addresses, supply check
```

Every dev account is pre-funded with 1,000,000 USDC on core. The faucet button adds more.

### Where prices come from

Every trade price comes from the order book: an order fills at the resting order's price, and nothing overrides that. The oracle is a separate reference index. It is used for:
- funding, which charges for the gap between the book and the oracle;
- the mark price, used for margin and liquidations;
- the order price band.

- **Oracle (`--oracle live`, the default):** `crates/hc-node/src/price_feed.rs` polls the best bid/ask on Binance, OKX, Bybit, Kraken, KuCoin, Gate, MEXC and Coinbase every 3 s. Hyperliquid uses its own spot market in the eighth slot; this devnet uses Coinbase. Each validator signs the weighted median (weights 3,2,2,1,1,1,1,1) as its oracle vote.
  - Genesis starts at the current spot prices. Those prices go in the block-log header, so replay needs no network.
  - If the feed is stale for 3 periods, validators stop voting and the oracle holds its last price.
  - `--oracle synthetic` uses a seeded random walk instead, for offline runs.
  - One poller serves all four in-process validators.

### Simulated market

`hc-devnet` starts a market simulator (`crates/hc-node/src/sim.rs`):
- **4 market makers (`mm-0`…`mm-3`):** each keeps a 14-level GTC ladder on both books. Every few blocks it cancels and requotes, leaning against its inventory.
- **20 takers (`taker-0`…`taker-19`):** they send IOC orders that cross the spread, about `--sim-trades-per-block` (default 100) per block, leaning against their own positions.
- **Fair price:** comes from the book. Makers quote around the current mid blended with the last trade, so order-flow imbalance moves the price. The oracle acts as the external market:
  - each update closes 5% of the gap between book and oracle;
  - takers lean toward selling a rich book and buying a cheap one.

  In a 3,000-block test, the last trade stayed within 0.3 bps of the oracle on average (sd 5 bps). The remaining gap is the premium that funding charges for.
- **No gaps in the book:** each refresh posts a new ladder and cancels the one before the previous ladder. A block runs cancels before new orders, so cancel-then-repost would leave an empty book for takers to sweep.
- **Funding:** accounts top themselves up with the faucet.

Pass `--sim-trades-per-block 0` for a quiet chain. At 100 trades per block the block log grows about 0.5 GB per hour. The explorer keeps the last 10,000 blocks of core transactions; the block log keeps everything.

### Scripted end-to-end demo (standard tooling)

```sh
cargo build --release && ./scripts/demo.sh
```

It runs these steps against the live chain:
1. Alice quotes and Bob takes on the book.
2. Alice bridges 5,000 USDC to the EVM.
3. Alice sends Carol 1,000 USDC with `cast send`.
4. `forge create` deploys `PerpBot`, which reads the core mark price through a precompile.
5. The bot funds its own core account through the bridge.
6. The bot places a core order via CoreWriter, and Bob fills it.
7. Carol bridges 500 USDC back to core.
8. The supply check shows a discrepancy of 0.
9. The explorer shows Alice's on-chain activity.
10. `hc-replay` replays the block log and reproduces the state hash.

### CLI and Foundry

```sh
B=./target/release
$B/hc-cli --user alice order BTC sell 100100 0.5 --tif gtc
$B/hc-cli --user bob market BTC buy 0.3
$B/hc-cli --user bob send-to-evm 1000
$B/hc-cli --user bob key                     # address + private key for cast
cast balance <addr> --rpc-url http://127.0.0.1:8080/evm
cast send 0x2222222222222222222222222222222222222222 --value 10ether \
  --private-key <key> --rpc-url http://127.0.0.1:8080/evm --gas-price 0 --legacy   # EVM -> core
$B/hc-cli --user bob leverage BTC 10 --isolated   # isolated margin on BTC
$B/hc-cli --user bob isolated-margin BTC 250       # add 250 USDC to it (negative removes)
$B/hc-cli --user bob twap BTC buy 0.5 10 --randomize
$B/hc-cli --user bob big-blocks true               # EVM transactions go to big blocks
$B/hc-cli --user bob supply
$B/hc-replay devnet-blocks.jsonl             # re-execute from the last snapshot (or genesis), check every state hash
```

In MetaMask, add network RPC `http://127.0.0.1:8080/evm` with chain id `31337` and symbol USDC.

### Restarting without losing state

By default `hc-devnet` starts a new chain. With `--resume` it continues the
chain in `--log`: balances, positions, open orders and EVM state carry over.

- Every 2,000 blocks (`--snapshot-blocks`) the executor writes the state to
  `devnet-blocks.jsonl.snapshot`, then starts a new log segment and deletes the
  old one. At 20k trades/s a full log grows about 2.5 GB an hour; with rotation
  it stays a few hundred MB. `--full-log` keeps the whole log from genesis
  (`hc-replay --from-genesis` then re-executes all of it).
- On `--resume`, the snapshot's own state hash is checked, the blocks logged
  after it are re-executed with every state hash checked, and a partly written
  last line (from a crash) is dropped. Consensus starts fresh (new views)
  and block heights continue.
- Chart candles are saved next to the log at every snapshot
  (`devnet-blocks.jsonl.candles`), so the chart keeps its history. The
  explorer's block and transaction history is rebuilt only from the blocks
  after the snapshot.

### One validator per process

```sh
scripts/multi-process.sh start      # 4 hc-validator processes; APIs on :8180-:8183, p2p on :9100-:9103
scripts/multi-process.sh check      # every node's state hash at the same height
scripts/multi-process.sh kill 3     # the other three keep finalizing (4 = 3f + 1 with f = 1)
scripts/multi-process.sh revive 3   # node 3 catches up and rejoins while the others keep running
scripts/multi-process.sh resume     # or restart all; a node that is behind downloads what it missed
scripts/multi-process.sh stop
```

Each `hc-validator` process runs one validator: consensus, mempool, executor,
state, block log, snapshots, explorer and API. Nodes share only a genesis file
(`hc-validator genesis`) and talk only over the network:
- consensus messages and block proposals over authenticated, encrypted p2p;
- **transaction gossip** on a separate p2p channel: a transaction sent to any
  node's API reaches every validator's mempool, batched every 5 ms;
- **block sync** on restart: a node that is behind asks its peers' APIs for the
  finalized blocks it missed, re-executes them checking every state hash, and
  starts consensus only once at least f other validators report the same
  state hash.

**Rejoining a running chain** (`revive`, `hc-validator run --resume --rejoin`):
the node catches up as above, then starts consensus from the **finalization
certificate** of the last block it has (simplex's `Floor::Finalized`) instead
of genesis. Every block's certificate and consensus digest are in the block
log for this. Tested: after a revived node rejoins, killing a different node
still leaves 3 voting validators, so the revived one is voting.

The nodes could run on separate machines with the same flags (`--peers`,
`--peer-apis`); this has only been tested on one machine.

**State sync:** peers keep only the blocks since their last snapshot. A node
that was down longer downloads a peer's snapshot (`GET /snapshot`), checks it
hashes to its recorded state hash and that f + 1 validators report that hash
at its height, then replays the blocks after it. Tested with
`SNAPSHOT=100 scripts/multi-process.sh start` (snapshots every 100 blocks).

## Performance

The numbers below were measured on one M4 Max (16 cores).

**Benchmark** (`cargo test --release -p hc-node --test throughput -- --ignored --nocapture`): 200,000 signed orders from 2,000 accounts.

| Step | Result |
|---|---|
| Signature recovery (EIP-712 digest of the msgpack action, then secp256k1), one core | k256 94 µs/tx; libsecp256k1 19 µs/tx |
| Signature recovery, 16 cores | 1.6 µs/tx (about 620k tx/s) |
| Execution (matching, margin, settlement, state hash) | 1.3 µs/tx (about 750k tx/s on one core) |

**Live devnet** (`cargo run --release --bin hc-load -- --rates 100000,150000,200000`): pre-signed WebSocket orders against the full pipeline on the same machine as the load generator. The pipeline is the API, consensus among N validators, the executor, result delivery, the block log and the explorer history.

| Setup | Offered load | Executed | p50 latency | p99 latency |
|---|---|---|---|---|
| 4 validators, 20k-tx blocks | 150k tx/s | 148k tx/s | 258 ms | 346 ms |
| 4 validators, 40k-tx blocks | 200k tx/s | 195k tx/s | 330 ms | 498 ms |
| 4 validators, 40k-tx blocks | 250k tx/s | 231k tx/s (ceiling) | 658 ms | 1.2 s |
| 22 validators, 60k-tx blocks | 100k tx/s | 93k tx/s | 1.1 s | 1.5 s |
| 22 validators, 25k-tx blocks, 14 consensus threads, `HC_BENCH_NO_FSYNC=1` | 100k tx/s | 99k tx/s | 265 ms | 475 ms |

How to read the 22-validator rows:
- All 22 validators share one SSD. Each one must flush its consensus journal to disk before voting, and on macOS each flush is a full-drive `F_FULLFSYNC`. The flushes queue behind each other, so rounds slow to about 2 per second. That is why the row with real flushes needs big blocks and has about 1 s latency.
- `HC_BENCH_NO_FSYNC=1` (benchmark only) skips those flushes, to approximate one disk per validator. It comes from a patched copy of `commonware-runtime` in `vendor/`. It is unsafe: a crash can lose votes.
- In-process validators share one mempool, one signer cache and one cache of checked proposals. A real node does that per-transaction work once, in parallel with the others. Consensus (votes, proposal broadcast to every peer, journals) runs in full for all N validators.

Relevant flags: `--validators N` (default 4; BFT tolerates f faults with n ≥ 3f + 1), `--max-block-txs` (default 20,000), `--max-block-bytes` (default 4 MiB; a signed order is about 90 bytes) and `--consensus-threads` (default: half the cores).

How the node gets there:
- Each signature is recovered once, at submission, and cached by transaction hash. Proposal checks and execution read the cache. Recovery is a pure function, so using the cache cannot change the result.
- A transaction leaves the mempool as soon as consensus finalizes its block, and a peer's proposal marks its transactions as in flight. So no transaction is proposed twice.
- The consensus runtime gets half the cores. It used to have 2 threads, which capped the block rate under load.
- The executor only executes. The block log, result delivery and the explorer history each run on their own thread.
- WebSocket orders are decoded once, straight into a transaction.
- Cancel-by-cloid uses an index, and margin checks borrow account state instead of copying it.

## Layout

| Crate | What |
|---|---|
| `hc-core` | Pure state machine `(State, block) -> (State', output)`: integer fixed point, ordered maps, no clocks. Covers the order book, clearinghouse (cross and isolated margin), oracle, funding, liquidation and ADL, TP/SL, TWAP, risk limits, signing (EIP-712 over msgpack), and EVM integration (`evm_bridge`: block queues, CoreWriter). |
| `hc-evm` | The HyperEVM-like revm fork: deterministic world state, precompiles, CoreWriter, bridge tracer, burned gas, tx decoding. Includes `contracts/PerpBot.sol`. |
| `hc-consensus` | Commonware simplex wiring: payload dissemination, ordered finalization, deterministic simulation, live TCP mode (all validators in one process, or one per process with a gossip channel). |
| `hc-node` | Mempools, transaction gossip, oracle feeders, executor, block log and snapshots, history, API, Ethereum RPC, UI (`src/ui/`), and the binaries `hc-devnet`, `hc-validator`, `hc-cli`, `hc-load` and `hc-replay`. |

The learning guide in `../docs/learn/` explains how the whole system works, with diagrams.

## Block execution
1. **Begin block:** set the height; block time = max(previous time, proposer time).
2. **Deliver core actions,** sorted into categories with proposer order kept within each:
   1. actions with no GTC/IOC order;
   2. cancels;
   3. GTC/IOC orders.

   Each action needs a valid signature (EIP-712, see above) and nonce (the 100 highest per signer are kept).
3. **EVM:** the block's raw Ethereum transactions join the small- or big-block queue. When a new second starts, a small block (2M gas) runs; when a new minute starts, a big one (30M gas). Each runs against a snapshot of core state. Then value sent to the bridge is credited to core, CoreWriter cancels run, and CoreWriter orders wait 3 s. Orders whose wait is over run next.
4. **End block:**
   - oracle aggregation (needs >2/3 of stake) and mark price update;
   - TP/SL triggers, then due TWAP slices;
   - a premium sample every 5 s;
   - hourly funding (the per-position rounding residual goes to the fee collector, so no USDC is created or destroyed);
   - risk limits;
   - liquidations: each isolated position on its own, then each cross account. Market orders while equity is above 2/3 of maintenance margin, the backstop vault below that, ADL below zero.
5. **Commit:** `state_hash = blake3(postcard(State))`. The EVM accounts, code and storage are part of it.

## Tests

```sh
cargo test --workspace --release       # 77 tests: engine, margin modes and ADL, TWAP, EVM blocks, agents, scenarios, replay,
                                       # restart from snapshots, signing vectors, consensus, EVM, market simulator
node scripts/check-js-signer.mjs <dir with viem@2.21, @noble/hashes@1.6, @nktkas/hyperliquid>  # Rust == browser == Hyperliquid SDK signatures
# (viem 2.57+ rejects the struct name "HyperliquidTransaction:ApproveAgent"; the UI loads 2.21 too)
# Headless browser checks against a running devnet (<dir> has playwright-core):
node scripts/ui-smoke.mjs <dir> [outdir]          # trading flow, bridge, explorer supply check
node scripts/ui-tpsl.mjs <dir> [outdir]           # TP/SL fires, sibling cancels, risk limit rejects
node scripts/ui-tpsl-partial.mjs <dir> [outdir]   # partial TP/SL levels next to whole-size ones
node scripts/ui-tpsl-typing.mjs <dir>             # typing survives live account updates
node scripts/ui-margin-twap.mjs <dir> [outdir]    # isolated margin add/remove, TWAP slices and cancel, big-blocks switch
node scripts/ui-explorer.mjs <dir> [outdir]       # transaction page tabs, end-of-block execution
node scripts/ui-vaults.mjs <dir> [outdir]         # create a vault, deposit, lockup refusal, trade as the vault
node scripts/ui-browser-wallet.mjs <dir> [outdir] # MetaMask stand-in approves an agent once, trades without prompts, signs a usdSend
```

Replay and determinism coverage:
- 3,000 random blocks executed twice match at every height.
- Encode → decode → replay reproduces the hashes.
- 4 validators reach identical state hashes over 150 consensus-finalized blocks with packet loss, and also with one validator down.
- A chain restarts from its log and snapshot exactly: after log rotation, after a crash mid-write, and after a resumed run appended more blocks. A tampered snapshot or log line is rejected.
- EVM runs are deterministic.
- A live devnet log replays exactly, including one with the simulator running.
- 400 blocks of simulated trading conserve USDC exactly through fees and funding, and two runs end at the same state hash.

## Not included (MVP scope)
- trailing stops, spot markets, HIP-3 markets, the protocol vault (HLP) as a depositable vault, and withdrawals to another chain (`withdraw3`)
- staking and validator set changes; the set is fixed at genesis
- deployment across machines (supported by the flags, but only tested on one machine)
