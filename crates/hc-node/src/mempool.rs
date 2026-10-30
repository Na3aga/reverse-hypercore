//! Per-validator mempool and the consensus [`PayloadSource`] built on it.
//!
//! Transactions are gossiped to every validator's mempool. A leader proposes
//! the oldest transactions that are not already in flight: ones it proposed
//! itself, or saw in another leader's proposal, within the last
//! `REPROPOSE_AFTER_MS`. They leave the pool as soon as consensus finalizes a
//! block that includes them (before execution), so a transaction proposed in
//! a nullified view is proposed again later, but an executed one never is.

use hc_consensus::PayloadSource;
use hc_core::action::RawEvmTx;
use hc_core::types::Hash32;
use hc_core::{BlockBody, SignedTx};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

/// Re-propose a transaction if it was proposed this long ago and is still pending.
const REPROPOSE_AFTER_MS: u64 = 2_000;
/// Finalized hashes remembered to drop late duplicates.
const FINALIZED_MEMORY: usize = 2_000_000;
/// Default cap on an encoded block, well under consensus's
/// [`hc_consensus::MAX_PAYLOAD_BYTES`]: big blocks take longer to broadcast
/// to every peer, which delays every validator's vote.
pub const DEFAULT_MAX_BLOCK_BYTES: usize = 4 << 20;
/// Room for the block's own fields (time, vector lengths).
const BODY_OVERHEAD: usize = 32;

/// Encoded size of a value inside a block body.
fn encoded_len<T: serde::Serialize>(v: &T) -> usize {
    let mut buf = [0u8; 1024];
    match postcard::to_slice(v, &mut buf) {
        Ok(b) => b.len(),
        Err(_) => postcard::to_stdvec(v).map_or(usize::MAX, |b| b.len()),
    }
}

#[derive(Default)]
struct Inner {
    next_seq: u64,
    /// Not in flight, by arrival order: what the next proposal takes from.
    ready: BTreeMap<u64, (Hash32, Arc<SignedTx>)>,
    /// Proposed by this validator or seen in a peer's proposal, with the time.
    in_flight: BTreeMap<u64, (Hash32, Arc<SignedTx>, u64)>,
    by_hash: HashMap<Hash32, u64>,
    last_sweep_ms: u64,
    finalized: HashSet<Hash32>,
    finalized_order: VecDeque<Hash32>,
    /// Raw Ethereum transactions, by arrival order (low volume: simple scan).
    evm_queue: BTreeMap<u64, (Hash32, Vec<u8>)>,
    evm_proposed_at: HashMap<Hash32, u64>,
}

impl Inner {
    fn admit(&mut self, h: Hash32, tx: Arc<SignedTx>) {
        if self.by_hash.contains_key(&h) || self.finalized.contains(&h) {
            return;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.ready.insert(seq, (h, tx));
        self.by_hash.insert(h, seq);
    }

    /// Returns transactions whose proposal went stale to the ready set.
    fn sweep(&mut self, now_ms: u64) {
        if now_ms.saturating_sub(self.last_sweep_ms) < REPROPOSE_AFTER_MS / 4 {
            return;
        }
        self.last_sweep_ms = now_ms;
        let stale: Vec<u64> = self.in_flight.iter().filter(|(_, (_, _, t))| now_ms.saturating_sub(*t) >= REPROPOSE_AFTER_MS).map(|(s, _)| *s).collect();
        for seq in stale {
            if let Some((h, tx, _)) = self.in_flight.remove(&seq) {
                self.ready.insert(seq, (h, tx));
            }
        }
    }
}

pub struct Mempool {
    inner: Mutex<Inner>,
    /// Arrivals not yet merged into `inner`. Submitters only hold this lock
    /// for a push, so they never wait behind block building.
    incoming: Mutex<Vec<(Hash32, Arc<SignedTx>)>>,
    pub max_txs_per_block: usize,
    /// Most bytes of encoded transactions a block may hold.
    pub max_block_bytes: usize,
}

impl Mempool {
    pub fn new(max_txs_per_block: usize) -> Arc<Mempool> {
        Mempool::with_limits(max_txs_per_block, DEFAULT_MAX_BLOCK_BYTES)
    }

    pub fn with_limits(max_txs_per_block: usize, max_block_bytes: usize) -> Arc<Mempool> {
        Arc::new(Mempool { inner: Mutex::default(), incoming: Mutex::default(), max_txs_per_block, max_block_bytes })
    }

    /// Admits a signed transaction after checking its signature.
    pub fn add(&self, tx: SignedTx) -> bool {
        if !tx.verify() {
            return false;
        }
        self.add_verified(tx.hash(), Arc::new(tx));
        true
    }

    /// Queues a transaction whose signature the caller already checked.
    /// Duplicates are dropped when the queue is merged.
    pub fn add_verified(&self, h: Hash32, tx: Arc<SignedTx>) {
        self.incoming.lock().expect("mempool incoming").push((h, tx));
    }

    /// Locks the pool with all queued arrivals merged in.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        let arrivals = std::mem::take(&mut *self.incoming.lock().expect("mempool incoming"));
        let mut m = self.inner.lock().expect("mempool lock");
        for (h, tx) in arrivals {
            m.admit(h, tx);
        }
        m
    }

    /// Admits a raw Ethereum transaction (signature and chain id checked).
    /// Returns its hash, or `None` for invalid or duplicate transactions.
    pub fn add_evm(&self, raw: Vec<u8>) -> Option<Hash32> {
        let decoded = hc_evm::decode_tx(&raw)?;
        if decoded.chain_id != hc_evm::CHAIN_ID {
            return None;
        }
        let h = Hash32(decoded.tx_hash);
        let mut m = self.lock();
        if m.by_hash.contains_key(&h) || m.finalized.contains(&h) {
            return None;
        }
        let seq = m.next_seq;
        m.next_seq += 1;
        m.evm_queue.insert(seq, (h, raw));
        m.by_hash.insert(h, seq);
        Some(h)
    }

    /// Selects EVM transactions in arrival order (same re-proposal rule),
    /// within `budget` encoded bytes.
    pub fn select_evm(&self, now_ms: u64, mut budget: usize) -> Vec<RawEvmTx> {
        let mut m = self.lock();
        let picked: Vec<(Hash32, Vec<u8>)> = m
            .evm_queue
            .values()
            .filter(|(h, _)| m.evm_proposed_at.get(h).is_none_or(|t| now_ms.saturating_sub(*t) >= REPROPOSE_AFTER_MS))
            .take(self.max_txs_per_block)
            .take_while(|(_, raw)| {
                let n = raw.len() + 4;
                budget = match budget.checked_sub(n) {
                    Some(b) => b,
                    None => return false,
                };
                true
            })
            .cloned()
            .collect();
        for (h, _) in &picked {
            m.evm_proposed_at.insert(*h, now_ms);
        }
        picked.into_iter().map(|(_, raw)| RawEvmTx(raw)).collect()
    }

    /// Takes ready transactions in arrival order, up to `max_txs_per_block`
    /// and `budget` encoded bytes, and marks them in flight. Returns them and
    /// the bytes they use. A transaction that does not fit waits for the next
    /// block, keeping arrival order.
    pub fn select(&self, now_ms: u64, budget: usize) -> (Vec<SignedTx>, usize) {
        let mut m = self.lock();
        m.sweep(now_ms);
        let mut out = Vec::with_capacity(self.max_txs_per_block.min(m.ready.len()));
        let mut used = 0;
        while out.len() < self.max_txs_per_block {
            let Some(entry) = m.ready.first_entry() else { break };
            let n = encoded_len(&*entry.get().1);
            if used + n > budget {
                break;
            }
            used += n;
            let (seq, (h, tx)) = entry.remove_entry();
            out.push((*tx).clone());
            m.in_flight.insert(seq, (h, tx, now_ms));
        }
        (out, used)
    }

    /// The transactions of the next block this validator proposes.
    pub fn build_body(&self, time_ms: u64) -> BlockBody {
        let budget = self.max_block_bytes.saturating_sub(BODY_OVERHEAD);
        let (txs, used) = self.select(time_ms, budget);
        BlockBody { time_ms, txs, evm_txs: self.select_evm(time_ms, budget - used) }
    }

    /// Marks transactions seen in a peer's proposal as in flight, so this
    /// validator does not propose them again while that block is pending.
    pub fn mark_in_flight(&self, hashes: &[Hash32], now_ms: u64) {
        let mut m = self.lock();
        for h in hashes {
            let Some(&seq) = m.by_hash.get(h) else { continue };
            if let Some((h, tx)) = m.ready.remove(&seq) {
                m.in_flight.insert(seq, (h, tx, now_ms));
            } else if let Some(e) = m.in_flight.get_mut(&seq) {
                e.2 = now_ms;
            }
        }
    }

    /// Removes finalized transactions.
    pub fn remove_finalized(&self, hashes: &[Hash32]) {
        let mut m = self.lock();
        for h in hashes {
            if let Some(seq) = m.by_hash.remove(h) {
                if m.ready.remove(&seq).is_none() && m.in_flight.remove(&seq).is_none() {
                    m.evm_queue.remove(&seq);
                }
            }
            m.evm_proposed_at.remove(h);
            if m.finalized.insert(*h) {
                m.finalized_order.push_back(*h);
            }
        }
        while m.finalized_order.len() > FINALIZED_MEMORY {
            if let Some(old) = m.finalized_order.pop_front() {
                m.finalized.remove(&old);
            }
        }
    }

    pub fn len(&self) -> usize {
        let m = self.lock();
        m.ready.len() + m.in_flight.len() + m.evm_queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Adapts a validator's mempool to the consensus layer.
pub struct ValidatorSource {
    pub mempool: Arc<Mempool>,
    /// Added to the consensus clock to get block time (the deterministic
    /// runtime's clock starts at 0).
    pub time_offset_ms: u64,
    pub max_payload_txs: usize,
    /// Peers' proposals larger than this are refused.
    pub max_payload_bytes: usize,
    /// Signers already recovered (at submission or in an earlier proposal).
    pub signers: Arc<crate::sigcache::SignerCache>,
    /// Shared verdicts for in-process validators (see `make_sources`).
    pub checked: Option<Arc<CheckedPayloads>>,
}

/// Verdicts on proposal payloads, keyed by a hash of the payload bytes.
/// Validation is a pure function of the bytes, so a cached verdict is exactly
/// what checking again would return.
#[derive(Default)]
pub struct CheckedPayloads(Mutex<(HashMap<[u8; 32], bool>, VecDeque<[u8; 32]>)>);

impl CheckedPayloads {
    fn get(&self, k: &[u8; 32]) -> Option<bool> {
        self.0.lock().expect("checked lock").0.get(k).copied()
    }

    fn put(&self, k: [u8; 32], ok: bool) {
        let mut g = self.0.lock().expect("checked lock");
        if g.0.insert(k, ok).is_none() {
            g.1.push_back(k);
            while g.1.len() > 4_096 {
                if let Some(old) = g.1.pop_front() {
                    g.0.remove(&old);
                }
            }
        }
    }
}

impl PayloadSource for ValidatorSource {
    fn build(&self, _view: u64, _parent: [u8; 32], now_ms: u64) -> Vec<u8> {
        self.mempool.build_body(now_ms + self.time_offset_ms).encode()
    }

    /// Structural checks only: decodable, bounded, all signatures valid
    /// (checked in parallel; usually cache hits from submission).
    /// Semantic validity (nonces, margin) is decided during execution, where a
    /// failing transaction becomes an error response rather than an invalid block.
    fn validate(&self, _view: u64, payload: &[u8]) -> bool {
        let key = *blake3::hash(payload).as_bytes();
        if let Some(ok) = self.checked.as_ref().and_then(|c| c.get(&key)) {
            return ok;
        }
        let Some(b) = BlockBody::decode(payload) else { return false };
        let hashes: Vec<Hash32> = b.txs.iter().map(SignedTx::hash).collect();
        let ok = payload.len() <= self.max_payload_bytes
            && b.txs.len() <= self.max_payload_txs
            && b.evm_txs.len() <= self.max_payload_txs
            && self.signers.signers_hashed(&b.txs, &hashes).iter().all(Option::is_some)
            && b.evm_txs.iter().all(|t| hc_evm::decode_tx(&t.0).is_some());
        if ok {
            self.mempool.mark_in_flight(&hashes, b.time_ms);
        }
        if let Some(c) = &self.checked {
            c.put(key, ok);
        }
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hc_core::action::{OrderTypeWire, OrderWire};
    use hc_core::types::Tif;
    use hc_core::Action;

    fn order_tx(i: u64) -> SignedTx {
        let o = OrderWire { a: 0, b: i.is_multiple_of(2), p: "100000".into(), s: "0.01".into(), r: false, t: OrderTypeWire::limit(Tif::Gtc), c: None };
        SignedTx::sign(&crate::dev_user("alice").0, Action::order(vec![o]), 1_000 + i)
    }

    #[test]
    fn blocks_stay_under_the_byte_cap_and_keep_arrival_order() {
        let cap = 20_000;
        let pool = Mempool::with_limits(100_000, cap);
        let txs: Vec<SignedTx> = (0..1_000).map(order_tx).collect();
        for t in &txs {
            pool.add_verified(t.hash(), Arc::new(t.clone()));
        }
        let mut seen = Vec::new();
        for _ in 0..1_000 {
            let body = pool.build_body(0);
            if body.txs.is_empty() {
                break;
            }
            let len = body.encode().len();
            assert!(len <= cap, "block of {len} bytes over the {cap}-byte cap");
            // Full blocks are close to the cap: within one transaction of it.
            if seen.len() + body.txs.len() < txs.len() {
                assert!(len > cap - 2 * encoded_len(&txs[0]), "block of {len} bytes is underfilled");
            }
            seen.extend(body.txs);
        }
        assert_eq!(seen, txs);
    }
}
