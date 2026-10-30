//! Transaction gossip between validators in separate processes.
//!
//! A transaction submitted to one node's API goes into that node's mempool
//! and, through this module, into every other validator's: new transactions
//! are batched for a few milliseconds and sent over the consensus p2p
//! network's gossip channel (authenticated, encrypted). A node checks each
//! received transaction's signature before admitting it, and does not pass
//! it on again: every validator is directly connected to every other.

use crate::node::Shared;
use hc_core::action::RawEvmTx;
use hc_core::SignedTx;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

/// A new transaction to gossip.
pub enum Item {
    Core(SignedTx),
    Evm(Vec<u8>),
}

#[derive(Default, Serialize, Deserialize)]
struct Batch {
    txs: Vec<SignedTx>,
    evm: Vec<RawEvmTx>,
}

/// Longest a transaction waits before its batch is sent.
const BATCH_WINDOW: Duration = Duration::from_millis(5);
/// A batch is sent early once it holds this many bytes (well under the gossip message limit).
const BATCH_BYTES: usize = 1 << 20;

/// Wires `shared` to consensus gossip. Returns the [`hc_consensus::Gossip`]
/// to hand to [`hc_consensus::spawn_tokio_validator`].
pub fn start(shared: Arc<Shared>) -> hc_consensus::Gossip {
    let (item_tx, mut items) = tokio::sync::mpsc::unbounded_channel::<Item>();
    let (out_tx, outbound) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    assert!(shared.gossip.set(item_tx).is_ok(), "gossip started once");
    tokio::spawn(async move {
        while let Some(first) = items.recv().await {
            let mut batch = Batch::default();
            let mut bytes = 0;
            let deadline = tokio::time::Instant::now() + BATCH_WINDOW;
            let mut next = Some(first);
            loop {
                match next.take() {
                    Some(Item::Core(tx)) => {
                        bytes += 160;
                        batch.txs.push(tx);
                    }
                    Some(Item::Evm(raw)) => {
                        bytes += raw.len();
                        batch.evm.push(RawEvmTx(raw));
                    }
                    None => {}
                }
                if bytes >= BATCH_BYTES {
                    break;
                }
                match tokio::time::timeout_at(deadline, items.recv()).await {
                    Ok(Some(item)) => next = Some(item),
                    _ => break,
                }
            }
            if out_tx.send(postcard::to_stdvec(&batch).expect("batch encodes")).is_err() {
                return;
            }
        }
    });
    // Received batches are checked on their own thread, off the consensus runtime.
    let (in_tx, in_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::Builder::new()
        .name("hc-gossip-in".into())
        .spawn(move || {
            while let Ok(bytes) = in_rx.recv() {
                let Ok(batch) = postcard::from_bytes::<Batch>(&bytes) else {
                    tracing::warn!(len = bytes.len(), "undecodable gossip batch");
                    continue;
                };
                // Signatures are recovered here, once (in parallel); proposals and execution reuse them.
                let hashes: Vec<_> = batch.txs.iter().map(SignedTx::hash).collect();
                shared.signers.signers_hashed(&batch.txs, &hashes);
                for (h, tx) in hashes.into_iter().zip(batch.txs) {
                    shared.admit(h, tx);
                }
                for raw in batch.evm {
                    shared.admit_evm(raw.0);
                }
            }
        })
        .expect("spawn gossip thread");
    let inbound = Arc::new(move |bytes: Vec<u8>| {
        let _ = in_tx.send(bytes);
    });
    hc_consensus::Gossip { outbound, inbound }
}
