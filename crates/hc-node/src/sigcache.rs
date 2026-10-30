//! Signer cache: each transaction's signature is recovered once.
//!
//! Recovery is the most expensive step per transaction (tens of µs against a
//! few µs for matching). Without a cache a transaction is recovered at
//! submission, by every validator checking a proposal, and again during
//! execution. The cache is keyed by the transaction hash, which covers the
//! action, the nonce and the signature, so a cached signer can only be reused
//! for the identical transaction. Recovery is a pure function, so a cache hit
//! returns exactly what recovery would and execution stays deterministic.
//!
//! The in-process validators share one cache; on a real network each node
//! would keep its own.

use hc_core::types::{Address, Hash32};
use hc_core::SignedTx;
use rayon::prelude::*;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

const SHARDS: usize = 64;

#[derive(Default)]
struct Shard {
    map: HashMap<Hash32, Address>,
    order: VecDeque<Hash32>,
}

pub struct SignerCache {
    shards: Vec<Mutex<Shard>>,
    per_shard: usize,
}

impl Default for SignerCache {
    fn default() -> Self {
        SignerCache::new(2_000_000)
    }
}

impl SignerCache {
    /// A cache holding about `capacity` signers (oldest evicted first).
    pub fn new(capacity: usize) -> SignerCache {
        SignerCache { shards: (0..SHARDS).map(|_| Mutex::default()).collect(), per_shard: capacity.div_ceil(SHARDS).max(1) }
    }

    fn shard(&self, h: &Hash32) -> &Mutex<Shard> {
        &self.shards[h.0[0] as usize % SHARDS]
    }

    pub fn get(&self, h: &Hash32) -> Option<Address> {
        self.shard(h).lock().expect("sigcache lock").map.get(h).copied()
    }

    pub fn insert(&self, h: Hash32, a: Address) {
        let mut s = self.shard(&h).lock().expect("sigcache lock");
        if s.map.insert(h, a).is_none() {
            s.order.push_back(h);
            while s.order.len() > self.per_shard {
                if let Some(old) = s.order.pop_front() {
                    s.map.remove(&old);
                }
            }
        }
    }

    /// Recovers (or looks up) one transaction's signer.
    pub fn signer(&self, tx: &SignedTx) -> Option<Address> {
        self.signer_with_hash(&tx.hash(), tx)
    }

    /// `signer` for a caller that already has the transaction hash.
    pub fn signer_with_hash(&self, h: &Hash32, tx: &SignedTx) -> Option<Address> {
        if let Some(a) = self.get(h) {
            return Some(a);
        }
        let a = tx.signer()?;
        self.insert(*h, a);
        Some(a)
    }

    /// Signers for a block, in order. Lookups are serial (a hash and a map
    /// probe each); only cache misses are recovered in parallel, so a block of
    /// hits does not wake the thread pool.
    pub fn signers(&self, txs: &[SignedTx]) -> Vec<Option<Address>> {
        let hashes: Vec<Hash32> = txs.iter().map(SignedTx::hash).collect();
        self.signers_hashed(txs, &hashes)
    }

    /// `signers` with the transaction hashes already computed.
    pub fn signers_hashed(&self, txs: &[SignedTx], hashes: &[Hash32]) -> Vec<Option<Address>> {
        let mut out: Vec<Option<Address>> = hashes.iter().map(|h| self.get(h)).collect();
        let misses: Vec<usize> = (0..txs.len()).filter(|&i| out[i].is_none()).collect();
        if !misses.is_empty() {
            let recovered: Vec<Option<Address>> = misses.par_iter().with_min_len(32).map(|&i| txs[i].signer()).collect();
            for (&i, a) in misses.iter().zip(recovered) {
                if let Some(a) = a {
                    self.insert(hashes[i], a);
                }
                out[i] = a;
            }
        }
        out
    }
}
