//! In-memory chain history for the explorer and the Ethereum RPC: finalized
//! blocks, a transaction index (core actions and EVM transactions), and a
//! per-address activity feed (trades, funding, bridge transfers, liquidations),
//! and OHLCV candles built from trades.
//! Core transaction records and activity are pruned to a recent window.
//! Everything here is derived from finalized blocks; the authoritative record
//! is the block log, which `hc-replay` re-executes to the same state hashes.

use crate::views;
use hc_core::events::{Event, EvmTxSummary, TxResponse};
use hc_core::fixed::Px;
use hc_core::types::{Address, Hash32};
use hc_core::{Action, BlockBody, BlockOutput, SignedTx, State};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};

/// Candle intervals (name, length in ms). Sub-minute intervals are extra
/// to the public API's set, which starts at 1m, since devnet blocks are fast.
pub const CANDLE_INTERVALS: [(&str, u64); 8] =
    [("1s", 1_000), ("5s", 5_000), ("15s", 15_000), ("1m", 60_000), ("5m", 300_000), ("15m", 900_000), ("1h", 3_600_000), ("4h", 14_400_000)];
const MAX_CANDLES: usize = 2_000;
/// Blocks whose core transactions stay indexed (older ones are pruned).
const MAX_BLOCKS: usize = 10_000;
const MAX_ACTIVITY_PER_ADDRESS: usize = 1_000;
/// Events kept for execution traces, across the newest blocks. A block's
/// traces go once newer blocks push the total past this; its transactions
/// stay indexed. About 120 bytes per event.
const MAX_TRACE_EVENTS: usize = 4_000_000;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Candle {
    /// Open time (ms).
    pub t: u64,
    pub open: Px,
    pub high: Px,
    pub low: Px,
    pub close: Px,
    /// Traded size in lots.
    pub volume: u64,
    pub n: u32,
}

pub fn interval_ms(name: &str) -> Option<(usize, u64)> {
    CANDLE_INTERVALS.iter().position(|(n, _)| *n == name).map(|i| (i, CANDLE_INTERVALS[i].1))
}

pub fn candle_json(st: &State, asset: usize, interval: usize, c: &Candle) -> Value {
    let (name, len) = CANDLE_INTERVALS[interval];
    let meta = &st.metas[asset];
    json!({
        "t": c.t, "T": c.t + len - 1, "s": meta.name, "i": name,
        "o": views::px(c.open), "c": views::px(c.close), "h": views::px(c.high), "l": views::px(c.low),
        "v": views::sz(c.volume as i64, meta.sz_decimals), "n": c.n,
    })
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockSummary {
    pub height: u64,
    pub view: u64,
    pub proposer: u32,
    pub time: u64,
    pub state_hash: Hash32,
    pub n_txs: usize,
    pub n_evm_txs: usize,
    pub evm_block: Option<u64>,
    pub evm_block_hash: Option<Hash32>,
    pub tx_hashes: Vec<Hash32>,
    /// End-of-block events (oracle, triggers, funding, risk, liquidations),
    /// until the block's traces are pruned.
    #[serde(skip)]
    pub system_events: Option<Vec<Event>>,
}

/// An indexed transaction. Core transactions keep the compact signed
/// transaction and its response and are rendered to JSON on request, which
/// keeps the index small under heavy load.
#[derive(Clone, Debug)]
pub struct TxRecord {
    pub hash: Hash32,
    pub height: u64,
    pub time: u64,
    /// `"core"` or `"evm"`.
    pub kind: &'static str,
    pub user: Address,
    pub detail: TxDetail,
    /// Events this transaction caused, in execution order; `None` once pruned.
    pub events: Option<Vec<Event>>,
}

#[derive(Clone, Debug)]
pub enum TxDetail {
    Core { tx: Option<SignedTx>, response: TxResponse },
    Evm { action: Value, result: Value },
}

impl TxRecord {
    pub fn to_json(&self, st: &State) -> Value {
        let (action, result) = match &self.detail {
            TxDetail::Core { tx, response } => {
                let szds = tx.as_ref().map(|t| crate::node::order_szds(st, &t.action)).unwrap_or_default();
                let action = tx.as_ref().map(|t| serde_json::to_value(&t.action).expect("action json")).unwrap_or(Value::Null);
                (action, views::tx_response(st, response, &szds))
            }
            TxDetail::Evm { action, result } => (action.clone(), result.clone()),
        };
        let mut j = json!({"hash": self.hash, "height": self.height, "time": self.time, "kind": self.kind, "user": self.user, "action": action, "result": result});
        if let TxDetail::Core { tx: Some(t), .. } = &self.detail {
            j["nonce"] = json!(t.nonce);
            j["signature"] = json!(format!("0x{}", hc_core::types::hex_encode(&t.signature.0)));
            // What was signed: the msgpack action hash, wrapped as EIP-712 typed data.
            j["actionHash"] = json!(format!("0x{}", hc_core::types::hex_encode(&hc_core::action::action_hash(&t.action, t.nonce))));
            j["eip712Digest"] = json!(format!("0x{}", hc_core::types::hex_encode(&hc_core::action::signing_hash(&t.action, t.nonce))));
        }
        j
    }

    /// Execution details: every event, trades, and per-account changes.
    pub fn execution(&self, st: &State) -> Value {
        match &self.events {
            Some(ev) => views::execution(st, ev),
            None => Value::Null,
        }
    }
}

/// The action's `type` tag, as in its JSON form.
pub fn action_type(a: &Action) -> &'static str {
    match a {
        Action::Order { .. } => "order",
        Action::Cancel { .. } => "cancel",
        Action::CancelByCloid { .. } => "cancelByCloid",
        Action::UpdateLeverage { .. } => "updateLeverage",
        Action::UpdateIsolatedMargin { .. } => "updateIsolatedMargin",
        Action::TwapOrder { .. } => "twapOrder",
        Action::TwapCancel { .. } => "twapCancel",
        Action::EvmUserModify { .. } => "evmUserModify",
        Action::Faucet { .. } => "faucet",
        Action::SendToEvm { .. } => "sendToEvm",
        Action::SetOracle { .. } => "setOracle",
        Action::SetRiskLimits { .. } => "setRiskLimits",
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    pub height: u64,
    pub time: u64,
    pub kind: String,
    pub detail: Value,
}

#[derive(Default)]
pub struct History {
    pub blocks: Vec<BlockSummary>,
    pub txs: BTreeMap<Hash32, TxRecord>,
    /// EVM receipts by tx hash, with the EVM block that included them.
    pub evm_receipts: BTreeMap<Hash32, (u64, Hash32, EvmTxSummary)>,
    /// EVM block number -> (hash, timestamp s, tx hashes, gas used).
    pub evm_blocks: BTreeMap<u64, (Hash32, u64, Vec<Hash32>, u64)>,
    /// EVM blocks that were big (30M gas) rather than small (2M gas).
    pub big_evm_blocks: std::collections::BTreeSet<u64>,
    pub activity: BTreeMap<Address, Vec<Activity>>,
    /// (asset, interval index) -> candles, oldest first.
    pub candles: BTreeMap<(u32, usize), VecDeque<Candle>>,
    /// (height, events kept) of blocks that still have traces, oldest first.
    traced: VecDeque<(u64, usize)>,
    trace_events: usize,
    /// Save candles to this file every this many blocks, so a resumed chain
    /// keeps its chart (candles are not part of the state snapshot).
    pub candle_file: Option<(std::path::PathBuf, u64)>,
    /// Candles already include trades up to this height (loaded from the file).
    candles_through: u64,
}

/// Candles saved at a height: `(height, series)`, postcard-encoded.
type SavedCandles = (u64, Vec<((u32, usize), Vec<Candle>)>);

/// The candle file next to a block log.
pub fn candle_path(log: &std::path::Path) -> std::path::PathBuf {
    let mut p = log.as_os_str().to_owned();
    p.push(".candles");
    p.into()
}

impl History {
    /// Loads candles saved by an earlier run of the chain. Blocks up to the
    /// saved height then leave candles alone when replayed.
    pub fn load_candles(&mut self, path: &std::path::Path) -> Option<u64> {
        let (height, series): SavedCandles = postcard::from_bytes(&std::fs::read(path).ok()?).ok()?;
        self.candles = series.into_iter().map(|(k, v)| (k, v.into())).collect();
        self.candles_through = height;
        Some(height)
    }

    fn save_candles(&self, path: &std::path::Path, height: u64) -> std::io::Result<()> {
        let saved: SavedCandles = (height, self.candles.iter().map(|(k, v)| (*k, v.iter().copied().collect())).collect());
        let tmp = path.with_extension("candles.tmp");
        std::fs::write(&tmp, postcard::to_stdvec(&saved).expect("candles encode"))?;
        std::fs::rename(tmp, path)
    }

    pub fn record(&mut self, st: &State, view: u64, proposer: u32, body: &BlockBody, out: &BlockOutput) {
        let (height, time) = (out.height, out.time_ms);
        let mut tx_hashes = Vec::new();
        let by_hash: BTreeMap<Hash32, &SignedTx> = body.txs.iter().map(|t| (t.hash(), t)).collect();
        // Split the block's events into each transaction's and the end-of-block ones.
        let mut covered = vec![false; out.events.len()];
        let mut tx_events: BTreeMap<Hash32, Vec<Event>> = BTreeMap::new();
        for (h, a, b) in &out.spans {
            let span = &out.events[*a as usize..*b as usize];
            covered[*a as usize..*b as usize].iter_mut().for_each(|c| *c = true);
            tx_events.entry(*h).or_default().extend_from_slice(span);
        }
        let system_events: Vec<Event> = out.events.iter().zip(&covered).filter(|(_, c)| !**c).map(|(e, _)| e.clone()).collect();
        let n_traced = out.events.len();
        for (hash, user, resp) in &out.responses {
            let tx = by_hash.get(hash).copied();
            tx_hashes.push(*hash);
            let events = Some(tx_events.remove(hash).unwrap_or_default());
            self.txs.insert(*hash, TxRecord { hash: *hash, height, time, kind: "core", user: *user, detail: TxDetail::Core { tx: tx.cloned(), response: resp.clone() }, events });
            if !matches!(resp, TxResponse::Error { .. }) {
                if let Some(t) = tx {
                    self.push(*user, height, time, "action", json!({"tx": hash, "action": {"type": action_type(&t.action)}}));
                }
            }
        }
        let raw_evm: BTreeMap<Hash32, hc_evm::DecodedTx> = body.evm_txs.iter().filter_map(|raw| hc_evm::decode_tx(&raw.0)).map(|d| (Hash32(d.tx_hash), d)).collect();
        let mut evm_block = None;
        let mut pending_evm: Vec<EvmTxSummary> = Vec::new();
        for e in &out.events {
            match e {
                Event::EvmTx(r) => pending_evm.push(r.clone()),
                Event::EvmBlock { number, hash, gas_used, big, .. } => {
                    if *big {
                        self.big_evm_blocks.insert(*number);
                    }
                    let hashes: Vec<Hash32> = pending_evm.iter().map(|r| r.tx_hash).collect();
                    for r in pending_evm.drain(..) {
                        tx_hashes.push(r.tx_hash);
                        self.txs.insert(
                            r.tx_hash,
                            TxRecord {
                                hash: r.tx_hash,
                                height,
                                time,
                                kind: "evm",
                                user: r.from,
                                detail: TxDetail::Evm {
                                    action: evm_action_json(&r, raw_evm.get(&r.tx_hash)),
                                    result: json!({"success": r.success, "gasUsed": r.gas_used, "logs": r.logs.len(), "evmBlock": number}),
                                },
                                events: Some(tx_events.remove(&r.tx_hash).unwrap_or_default()),
                            },
                        );
                        self.push(r.from, height, time, "evmTx", json!({"tx": r.tx_hash, "to": r.to, "valueWei": r.value_wei, "success": r.success, "contractAddress": r.contract_address}));
                        if let Some(to) = r.to {
                            if to != r.from && r.value_wei != "0" {
                                self.push(to, height, time, "evmTransferIn", json!({"tx": r.tx_hash, "from": r.from, "valueWei": r.value_wei}));
                            }
                        }
                        self.evm_receipts.insert(r.tx_hash, (*number, *hash, r));
                    }
                    self.evm_blocks.insert(*number, (*hash, time / 1000, hashes, *gas_used));
                    evm_block = Some((*number, *hash));
                }
                Event::Fill(f) => {
                    self.push(f.user, height, time, "fill", views::fill(st, f));
                    // One candle update per trade: the taker's side of it.
                    if f.crossed && height > self.candles_through {
                        self.add_trade(f.asset, f.time, f.px, f.sz);
                    }
                }
                Event::Ledger { user, kind, usd } => {
                    self.push(*user, height, time, "ledger", json!({"kind": kind, "usdc": usd.to_string()}));
                }
                Event::Funding { user, asset, usdc, rate, .. } => {
                    self.push(*user, height, time, "funding", json!({"coin": st.metas[*asset as usize].name, "usdc": usdc.to_string(), "fundingRate": rate.to_string()}));
                }
                Event::Liquidation { user, method, account_value, .. } => {
                    self.push(*user, height, time, "liquidation", json!({"method": method, "accountValue": account_value.to_string()}));
                }
                Event::Risk { user, kind, account_value } => {
                    self.push(*user, height, time, "risk", json!({"kind": kind, "accountValue": account_value.to_string()}));
                }
                // Start and end of a TWAP; its slices show up as fills.
                Event::Twap(t) if t.status != hc_core::events::TwapStatus::Running => {
                    self.push(t.user, height, time, "twap", views::twap_event(st, t));
                }
                Event::CoreWriter { user, asset, response, tx_hash } => {
                    let szd = asset.and_then(|a| st.metas.get(a as usize)).map(|m| m.sz_decimals);
                    let mut detail = views::order_response(st, response, szd);
                    detail["tx"] = json!(tx_hash);
                    self.push(*user, height, time, "coreWriter", detail);
                }
                _ => {}
            }
        }
        self.blocks.push(BlockSummary {
            height,
            view,
            proposer,
            time,
            state_hash: out.state_hash,
            n_txs: body.txs.len(),
            n_evm_txs: body.evm_txs.len(),
            evm_block: evm_block.map(|(n, _)| n),
            evm_block_hash: evm_block.map(|(_, h)| h),
            tx_hashes,
            system_events: Some(system_events),
        });
        self.traced.push_back((height, n_traced));
        self.trace_events += n_traced;
        self.prune();
        if let Some((path, every)) = &self.candle_file {
            if *every > 0 && height % every == 0 {
                if let Err(e) = self.save_candles(path, height) {
                    tracing::warn!(height, error = %e, "saving candles failed");
                }
            }
        }
    }

    fn add_trade(&mut self, asset: u32, time: u64, px: Px, lots: u64) {
        for (i, (_, len)) in CANDLE_INTERVALS.iter().enumerate() {
            let t = time - time % len;
            let series = self.candles.entry((asset, i)).or_default();
            match series.back_mut() {
                Some(c) if c.t == t => {
                    c.high = c.high.max(px);
                    c.low = c.low.min(px);
                    c.close = px;
                    c.volume += lots;
                    c.n += 1;
                }
                _ => {
                    series.push_back(Candle { t, open: px, high: px, low: px, close: px, volume: lots, n: 1 });
                    if series.len() > MAX_CANDLES {
                        series.pop_front();
                    }
                }
            }
        }
    }

    /// Candles of `asset` updated by trades in a block at `time_ms` (the last
    /// candle of each series, if it covers that time).
    pub fn candle_at(&self, asset: u32, interval: usize, time_ms: u64) -> Option<&Candle> {
        let len = CANDLE_INTERVALS[interval].1;
        self.candles.get(&(asset, interval))?.back().filter(|c| c.t == time_ms - time_ms % len)
    }

    pub fn candles_between(&self, asset: u32, interval: usize, start: u64, end: u64) -> Vec<&Candle> {
        self.candles.get(&(asset, interval)).map(|s| s.iter().filter(|c| c.t >= start && c.t <= end).collect()).unwrap_or_default()
    }

    /// Keeps memory bounded under sustained load: old core transaction
    /// records go (EVM ones stay, for the Ethereum RPC), and so does old
    /// per-address activity.
    fn prune(&mut self) {
        while self.trace_events > MAX_TRACE_EVENTS && self.traced.len() > 1 {
            let (h, n) = self.traced.pop_front().expect("non-empty");
            self.trace_events -= n;
            if let Ok(i) = self.blocks.binary_search_by_key(&h, |b| b.height) {
                self.blocks[i].system_events = None;
                for t in &self.blocks[i].tx_hashes {
                    if let Some(r) = self.txs.get_mut(t) {
                        r.events = None;
                    }
                }
            }
        }
        if self.blocks.len() > MAX_BLOCKS + 1_000 {
            let old: Vec<BlockSummary> = self.blocks.drain(..self.blocks.len() - MAX_BLOCKS).collect();
            for b in old {
                for h in b.tx_hashes {
                    if self.txs.get(&h).is_some_and(|t| t.kind == "core") {
                        self.txs.remove(&h);
                    }
                }
            }
        }
        for a in self.activity.values_mut() {
            if a.len() > MAX_ACTIVITY_PER_ADDRESS + 500 {
                a.drain(..a.len() - MAX_ACTIVITY_PER_ADDRESS);
            }
        }
    }

    fn push(&mut self, user: Address, height: u64, time: u64, kind: &str, detail: Value) {
        self.activity.entry(user).or_default().push(Activity { height, time, kind: kind.into(), detail });
    }

    pub fn recent_blocks(&self, limit: usize, before: Option<u64>) -> Vec<&BlockSummary> {
        self.blocks.iter().rev().filter(|b| before.is_none_or(|h| b.height < h)).take(limit).collect()
    }
}

/// EVM transaction fields for the explorer, with the decoded raw transaction
/// when it is available (gas limit and price, type, calldata).
fn evm_action_json(r: &EvmTxSummary, d: Option<&hc_evm::DecodedTx>) -> Value {
    let mut j = json!({"to": r.to, "valueWei": r.value_wei, "contractAddress": r.contract_address, "nonce": r.nonce});
    if let Some(d) = d {
        let price = d.effective_gas_price();
        j["type"] = json!(d.tx_type);
        j["chainId"] = json!(d.chain_id);
        j["gasLimit"] = json!(d.gas_limit);
        j["gasPriceWei"] = json!(price.to_string());
        j["feeWei"] = json!((price * r.gas_used as u128).to_string());
        j["input"] = json!(format!("0x{}", hc_core::types::hex_encode(&d.input)));
    }
    j
}
