//! Executor: applies finalized blocks to the state, keeps an append-only block
//! log for replay, and publishes block outputs to API subscribers.

use crate::mempool::Mempool;
use hc_consensus::Finalized;
use hc_core::types::{hex_encode, Hash32};
use hc_core::{BlockBody, BlockOutput, Genesis, State};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::{broadcast, oneshot};

/// One line of the block log: enough to replay execution exactly.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEntry {
    pub height: u64,
    pub view: u64,
    pub proposer: u32,
    /// Hex of the consensus payload (a postcard-encoded [`BlockBody`]).
    pub payload: String,
    /// State hash after executing this block.
    pub state_hash: Hash32,
    /// Hex of the consensus digest of the block (empty in older logs).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub digest: String,
    /// Hex of the block's finalization certificate, when one named it. A
    /// validator rejoins a running chain from the last one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub certificate: String,
}

impl LogEntry {
    pub fn new(f: &Finalized, state_hash: Hash32) -> LogEntry {
        LogEntry {
            height: f.height,
            view: f.view,
            proposer: f.proposer,
            payload: hex_encode(&f.payload),
            state_hash,
            digest: if f.digest == [0; 32] { String::new() } else { hex_encode(&f.digest) },
            certificate: hex_encode(&f.certificate),
        }
    }

    /// The finalized block this line records.
    pub fn finalized(&self) -> Result<Finalized, String> {
        let hex = |s: &str, what: &str| hc_core::types::hex_decode(s).ok_or_else(|| format!("bad {what} hex at height {}", self.height));
        let mut digest = [0u8; 32];
        if !self.digest.is_empty() {
            digest.copy_from_slice(&hex(&self.digest, "digest")?.get(..32).ok_or("short digest")?[..32]);
        }
        Ok(Finalized { view: self.view, height: self.height, proposer: self.proposer, digest, payload: hex(&self.payload, "payload")?, certificate: hex(&self.certificate, "certificate")? })
    }
}

pub struct Shared {
    pub state: RwLock<State>,
    pub outputs: broadcast::Sender<Arc<BlockOutput>>,
    pub mempools: Vec<Arc<Mempool>>,
    /// Transaction hash -> waiters for its execution result.
    pub waiters: Waiters,
    pub history: RwLock<crate::history::History>,
    pub signers: Arc<crate::sigcache::SignerCache>,
    /// Set when validators run in separate processes: new transactions are
    /// also sent to the other validators (see [`crate::gossip`]).
    pub gossip: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<crate::gossip::Item>>,
    /// This node's block log, served to peers that catch up (see [`read_log`]).
    pub log_path: std::sync::OnceLock<std::path::PathBuf>,
}

impl Shared {
    pub fn new(genesis: &Genesis, mempools: Vec<Arc<Mempool>>, signers: Arc<crate::sigcache::SignerCache>) -> Arc<Shared> {
        Arc::new(Shared {
            state: RwLock::new(State::genesis(genesis)),
            outputs: broadcast::channel(1024).0,
            mempools,
            waiters: Waiters::default(),
            history: RwLock::new(crate::history::History::default()),
            signers,
            gossip: std::sync::OnceLock::new(),
            log_path: std::sync::OnceLock::new(),
        })
    }

    /// Gossips a transaction to every validator's mempool. The signature is
    /// recovered once here and cached for proposal checks and execution.
    pub fn submit(&self, tx: hc_core::SignedTx) -> bool {
        self.submit_hashed(tx.hash(), tx)
    }

    /// `submit` for a caller that already has the transaction hash.
    pub fn submit_hashed(&self, h: Hash32, tx: hc_core::SignedTx) -> bool {
        if !self.admit(h, tx.clone()) {
            return false;
        }
        if let Some(g) = self.gossip.get() {
            let _ = g.send(crate::gossip::Item::Core(tx));
        }
        true
    }

    /// Adds a transaction to the local mempools only (it came from a peer).
    pub fn admit(&self, h: Hash32, tx: hc_core::SignedTx) -> bool {
        if self.signers.signer_with_hash(&h, &tx).is_none() {
            return false;
        }
        let tx = Arc::new(tx);
        for m in &self.mempools {
            m.add_verified(h, tx.clone());
        }
        true
    }

    /// Adds a raw Ethereum transaction to the mempools and gossips it.
    /// Returns its hash if it was valid and new.
    pub fn submit_evm(&self, raw: Vec<u8>) -> Option<Hash32> {
        let h = self.admit_evm(raw.clone());
        if h.is_some() {
            if let Some(g) = self.gossip.get() {
                let _ = g.send(crate::gossip::Item::Evm(raw));
            }
        }
        h
    }

    pub fn admit_evm(&self, raw: Vec<u8>) -> Option<Hash32> {
        let mut hash = None;
        for m in &self.mempools {
            hash = m.add_evm(raw.clone()).or(hash);
        }
        hash
    }
}

type Waiter = oneshot::Sender<serde_json::Value>;

/// Waiters for transaction results, sharded by hash so concurrent requests
/// do not contend on one lock.
pub struct Waiters(Vec<Mutex<HashMap<Hash32, Vec<Waiter>>>>);

impl Default for Waiters {
    fn default() -> Self {
        Waiters((0..64).map(|_| Mutex::default()).collect())
    }
}

impl Waiters {
    fn shard(&self, h: &Hash32) -> std::sync::MutexGuard<'_, HashMap<Hash32, Vec<Waiter>>> {
        self.0[h.0[0] as usize % self.0.len()].lock().expect("waiters lock")
    }

    pub fn add(&self, h: Hash32, w: Waiter) {
        self.shard(&h).entry(h).or_default().push(w);
    }

    pub fn take(&self, h: &Hash32) -> Option<Vec<Waiter>> {
        self.shard(h).remove(h)
    }
}

/// Executes one finalized block. Undecodable payloads cannot be finalized
/// (validators refuse to vote for them), so decoding failures are a bug.
/// Signers come from `cache` when given, else are recovered in parallel.
pub fn execute(state: &mut State, f: &Finalized, cache: Option<&crate::sigcache::SignerCache>) -> (BlockBody, BlockOutput) {
    let body = BlockBody::decode(&f.payload).expect("finalized payloads are validated");
    let evm_txs: Vec<Vec<u8>> = body.evm_txs.iter().map(|t| t.0.clone()).collect();
    let signers = match cache {
        Some(c) => c.signers(&body.txs),
        None => hc_core::action::recover_signers(&body.txs),
    };
    let out = state.apply_block_with_signers(f.height, body.time_ms, &body.txs, &signers, &evm_txs);
    (body, out)
}

/// Drops finalized transactions from every mempool as soon as consensus
/// finalizes their block, then hands the block to the executor. Execution can
/// lag behind consensus under load; mempools must not wait for it, or leaders
/// keep proposing transactions that are already in a finalized block.
pub fn spawn_finalizer(
    shared: Arc<Shared>,
    mut finalized: tokio::sync::mpsc::UnboundedReceiver<Finalized>,
) -> tokio::sync::mpsc::UnboundedReceiver<Finalized> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(f) = finalized.recv().await {
            if let Some(body) = BlockBody::decode(&f.payload) {
                let mut hashes: Vec<Hash32> = body.txs.iter().map(|t| t.hash()).collect();
                hashes.extend(body.evm_txs.iter().filter_map(|t| hc_evm::decode_tx(&t.0)).map(|d| Hash32(d.tx_hash)));
                for m in &shared.mempools {
                    m.remove_finalized(&hashes);
                }
            }
            if tx.send(f).is_err() {
                return;
            }
        }
    });
    rx
}

/// A block after execution, handed to the post-processing threads.
struct Executed {
    f: Finalized,
    body: BlockBody,
    out: Arc<BlockOutput>,
    t_exec: std::time::Duration,
    queued: usize,
}

/// Where the executor writes the block log, and how often it snapshots state.
pub struct LogSink {
    /// Taken by the writer thread, so a rotated-out segment is closed (and
    /// its disk space freed) once the writer moves on.
    pub file: Option<std::fs::File>,
    pub path: std::path::PathBuf,
    /// Genesis header line of a new segment (without `snapshot_height`).
    pub header: serde_json::Value,
    /// Height the current segment starts after (0 = genesis).
    pub segment_start: u64,
    /// Bytes already in the current segment.
    pub offset: u64,
    /// Snapshot state every this many blocks (0 = never).
    pub snapshot_every: u64,
    /// After each snapshot, start a new segment and delete the old one.
    pub rotate: bool,
}

enum LogMsg {
    Block(Finalized, Hash32),
    /// postcard-encoded state after `height`.
    Snapshot { height: u64, state_hash: Hash32, state: Vec<u8> },
}

/// Snapshot file next to the log: a JSON line of [`SnapshotMeta`], then the
/// postcard-encoded [`State`].
pub fn snapshot_path(log: &std::path::Path) -> std::path::PathBuf {
    let mut p = log.as_os_str().to_owned();
    p.push(".snapshot");
    p.into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub genesis_time_ms: u64,
    pub height: u64,
    pub state_hash: Hash32,
    /// The log segment the snapshot was taken in, and its length after `height`.
    pub segment_start: u64,
    pub log_offset: u64,
}

/// Header line of a devnet block log: everything needed to rebuild genesis.
pub fn log_header(genesis: &Genesis) -> serde_json::Value {
    serde_json::json!({
        "genesis_time_ms": genesis.time_ms,
        "validators": genesis.validators.len(),
        "oracle_pxs": genesis.oracle_pxs.iter().map(|p| p.0).collect::<Vec<_>>(),
    })
}

impl LogSink {
    /// Starts a new chain's log at `path`: truncates it, writes the header,
    /// and deletes the snapshot and saved candles of an earlier chain.
    pub fn create(path: &std::path::Path, genesis: &Genesis, snapshot_every: u64, rotate: bool) -> std::io::Result<LogSink> {
        let header = log_header(genesis);
        let mut file = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(path)?;
        let line = format!("{header}\n");
        file.write_all(line.as_bytes())?;
        for old in [snapshot_path(path), crate::history::candle_path(path)] {
            match std::fs::remove_file(old) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        Ok(LogSink { file: Some(file), path: path.into(), header, segment_start: 0, offset: line.len() as u64, snapshot_every, rotate })
    }

    /// Continues the log of a chain rebuilt by [`load_chain`]: drops a partly
    /// written last line, then appends.
    pub fn resume(path: &std::path::Path, loaded: &Loaded, snapshot_every: u64, rotate: bool) -> std::io::Result<LogSink> {
        std::fs::OpenOptions::new().write(true).open(path)?.set_len(loaded.valid_len)?;
        let file = std::fs::OpenOptions::new().append(true).open(path)?;
        let mut header = loaded.header.clone();
        if let Some(h) = header.as_object_mut() {
            h.remove("snapshot_height");
        }
        Ok(LogSink { file: Some(file), path: path.into(), header, segment_start: loaded.segment_start, offset: loaded.valid_len, snapshot_every, rotate })
    }

    fn run(mut self, rx: std::sync::mpsc::Receiver<LogMsg>) {
        let mut w = std::io::BufWriter::with_capacity(8 << 20, self.file.take().expect("log file"));
        while let Ok(first) = rx.recv() {
            let mut next = Some(first);
            // Write everything queued, then flush, so the log is current whenever the chain is idle.
            while let Some(msg) = next {
                match msg {
                    LogMsg::Block(f, state_hash) => {
                        let entry = LogEntry::new(&f, state_hash);
                        let line = serde_json::to_string(&entry).expect("log entry encodes");
                        let _ = writeln!(w, "{line}");
                        self.offset += line.len() as u64 + 1;
                    }
                    LogMsg::Snapshot { height, state_hash, state } => {
                        let _ = w.flush();
                        if let Err(e) = self.snapshot(height, state_hash, &state) {
                            tracing::warn!(height, error = %e, "state snapshot failed");
                        } else if self.rotate {
                            match self.new_segment(height) {
                                Ok(file) => w = std::io::BufWriter::with_capacity(8 << 20, file),
                                Err(e) => tracing::warn!(height, error = %e, "block log rotation failed; continuing in the current segment"),
                            }
                        }
                    }
                }
                next = rx.try_recv().ok();
            }
            let _ = w.flush();
        }
    }

    fn snapshot(&self, height: u64, state_hash: Hash32, state: &[u8]) -> std::io::Result<()> {
        let meta = SnapshotMeta {
            genesis_time_ms: self.header["genesis_time_ms"].as_u64().unwrap_or_default(),
            height,
            state_hash,
            segment_start: self.segment_start,
            log_offset: self.offset,
        };
        write_snapshot(&self.path, &meta, state)
    }

    /// Replaces the log with a new segment holding the blocks after `height`.
    fn new_segment(&mut self, height: u64) -> std::io::Result<std::fs::File> {
        let (f, len) = start_segment(&self.path, &self.header, height)?;
        self.segment_start = height;
        self.offset = len;
        Ok(f)
    }
}

/// Writes the snapshot next to the log at `log` (atomically: temp file, then rename).
pub fn write_snapshot(log: &std::path::Path, meta: &SnapshotMeta, state: &[u8]) -> std::io::Result<()> {
    let path = snapshot_path(log);
    let tmp = path.with_extension("snapshot.tmp");
    let mut f = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
    writeln!(f, "{}", serde_json::to_string(meta).expect("meta encodes"))?;
    f.write_all(state)?;
    f.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    std::fs::rename(&tmp, &path)?;
    tracing::info!(height = meta.height, mb = format!("{:.1}", state.len() as f64 / 1e6), "state snapshot written");
    Ok(())
}

/// Replaces the log at `log` with a new segment for the blocks after
/// `height` (header = genesis header + `snapshot_height`). Returns the file,
/// open for appending, and its length.
pub fn start_segment(log: &std::path::Path, genesis_header: &serde_json::Value, height: u64) -> std::io::Result<(std::fs::File, u64)> {
    let tmp = log.with_extension("jsonl.tmp");
    let mut header = genesis_header.clone();
    if let Some(h) = header.as_object_mut() {
        h.remove("snapshot_height");
    }
    header["snapshot_height"] = height.into();
    let line = format!("{header}\n");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(line.as_bytes())?;
    std::fs::rename(&tmp, log)?;
    Ok((f, line.len() as u64))
}

/// A snapshot file's contents: its header and the postcard-encoded state.
pub fn parse_snapshot(bytes: &[u8]) -> Result<(SnapshotMeta, State), String> {
    let nl = bytes.iter().position(|b| *b == b'\n').ok_or("snapshot has no header line")?;
    let meta: SnapshotMeta = serde_json::from_slice(&bytes[..nl]).map_err(|e| format!("bad snapshot header: {e}"))?;
    let state: State = postcard::from_bytes(&bytes[nl + 1..]).map_err(|e| format!("bad snapshot state: {e}"))?;
    if state.height != meta.height || state.hash() != meta.state_hash {
        return Err(format!("snapshot at height {} does not match its recorded state hash", meta.height));
    }
    Ok((meta, state))
}

/// Consumes finalized blocks until the channel closes.
///
/// The executor thread only decodes, executes and hands off. Everything
/// downstream runs on its own threads so the next block can execute
/// meanwhile:
/// - the block log writer, which also writes state snapshots;
/// - result delivery to waiting requests (built in parallel), then the block
///   output broadcast and the explorer history.
///
/// Post-processing renders responses with a copy of the state taken at start:
/// it only reads asset metadata (names, size decimals), which never changes.
pub async fn run_executor(shared: Arc<Shared>, mut finalized: tokio::sync::mpsc::UnboundedReceiver<Finalized>, log: Option<LogSink>) {
    let meta_state = Arc::new(shared.state.read().expect("state lock").clone());
    let snapshot_every = log.as_ref().map_or(0, |l| l.snapshot_every);
    let (log_tx, log_rx) = std::sync::mpsc::channel::<LogMsg>();
    let log_thread = log.map(|sink| std::thread::Builder::new().name("hc-blocklog".into()).spawn(move || sink.run(log_rx)).expect("spawn block log thread"));
    let (post_tx, post_rx) = std::sync::mpsc::channel::<Executed>();
    {
        let shared = shared.clone();
        std::thread::Builder::new().name("hc-post".into()).spawn(move || post_process(&shared, &meta_state, post_rx)).expect("spawn post thread");
    }
    while let Some(f) = finalized.recv().await {
        let t0 = std::time::Instant::now();
        let (body, out, snapshot) = {
            let mut st = shared.state.write().expect("state lock");
            let (body, out) = execute(&mut st, &f, Some(&shared.signers));
            // Encoding costs about as much as the state hash every block computes.
            let snapshot = (snapshot_every > 0 && f.height % snapshot_every == 0).then(|| postcard::to_stdvec(&*st).expect("state encodes"));
            (body, out, snapshot)
        };
        let t_exec = t0.elapsed();
        let (height, state_hash) = (f.height, out.state_hash);
        let _ = log_tx.send(LogMsg::Block(f.clone(), state_hash));
        if let Some(state) = snapshot {
            let _ = log_tx.send(LogMsg::Snapshot { height, state_hash, state });
        }
        let _ = post_tx.send(Executed { f, body, out: Arc::new(out), t_exec, queued: finalized.len() });
    }
    // The chain stopped: let the log writer finish, so the log and snapshot are complete on return.
    drop(log_tx);
    if let Some(t) = log_thread {
        let _ = tokio::task::spawn_blocking(move || t.join()).await;
    }
}

fn post_process(shared: &Arc<Shared>, st: &Arc<State>, blocks: std::sync::mpsc::Receiver<Executed>) {
    use rayon::prelude::*;
    // Explorer history on its own thread: it is the slowest consumer and only
    // the explorer reads it, so it must not delay results.
    let (hist_tx, hist_rx) = std::sync::mpsc::channel::<(u64, u32, BlockBody, Arc<BlockOutput>)>();
    {
        let (shared, st) = (shared.clone(), st.clone());
        std::thread::Builder::new()
            .name("hc-history".into())
            .spawn(move || {
                while let Ok((view, proposer, body, out)) = hist_rx.recv() {
                    shared.history.write().expect("history lock").record(&st, view, proposer, &body, &out);
                }
            })
            .expect("spawn history thread");
    }
    let ms = |d: std::time::Duration| format!("{:.1}", d.as_secs_f64() * 1e3);
    while let Ok(Executed { f, body, out, t_exec, queued }) = blocks.recv() {
        let t0 = std::time::Instant::now();
        // Wake HTTP/WS callers waiting on their transactions.
        out.responses.par_iter().enumerate().with_min_len(512).for_each(|(i, (h, _, resp))| {
            if let Some(ws) = shared.waiters.take(h) {
                let v = crate::views::tx_response(st, resp, &order_szds(st, &body.txs[i].action));
                for w in ws {
                    let _ = w.send(v.clone());
                }
            }
        });
        let t_wake = t0.elapsed();
        let _ = shared.outputs.send(out.clone());
        let (txs, height, view, hash) = (body.txs.len(), f.height, f.view, out.state_hash);
        let _ = hist_tx.send((f.view, f.proposer, body, out));
        if height % 100 == 0 || txs > 0 {
            tracing::info!(
                height,
                view,
                txs,
                exec_ms = ms(t_exec),
                wake_ms = ms(t_wake),
                queued,
                state_hash = ?hash,
                "executed block"
            );
        }
    }
}

/// Size decimals for each order in an action (to format filled sizes).
pub fn order_szds(state: &State, action: &hc_core::Action) -> Vec<Option<u32>> {
    match action {
        hc_core::Action::Order { orders, .. } => orders.iter().map(|o| state.metas.get(o.a as usize).map(|m| m.sz_decimals)).collect(),
        _ => vec![],
    }
}

/// A chain rebuilt from its block log (and snapshot, if the log was rotated).
pub struct Loaded {
    pub header: serde_json::Value,
    pub genesis: Genesis,
    pub state: State,
    /// Height the state was loaded at (snapshot or genesis), before replay.
    pub start_height: u64,
    pub segment_start: u64,
    /// Length of the log up to its last complete line.
    pub valid_len: u64,
    /// The last logged block, if it has a finalization certificate:
    /// (height, certificate). A validator rejoins a running chain from it.
    pub last_certificate: Option<(u64, Vec<u8>)>,
}

/// Genesis of a devnet log from its header line.
pub fn genesis_from_header(header: &serde_json::Value) -> Result<Genesis, String> {
    let time = header["genesis_time_ms"].as_u64().ok_or("log header has no genesis_time_ms")?;
    let n = header["validators"].as_u64().map_or(crate::N_VALIDATORS, |n| n as usize);
    let mut genesis = crate::devnet_genesis_n(time, n);
    if let Some(pxs) = header["oracle_pxs"].as_array() {
        genesis.oracle_pxs = pxs.iter().map(|p| p.as_u64().map(hc_core::fixed::Px)).collect::<Option<_>>().ok_or("bad oracle_pxs in log header")?;
    }
    Ok(genesis)
}

/// Rebuilds the chain in the block log at `path`: from the snapshot next to
/// it when there is a usable one (and `use_snapshot`, or the log was rotated
/// so genesis is not an option), else from genesis; then re-executes every
/// later block, checking each state hash, and calls `on_block` for it.
/// Returns `None` for a missing or empty log. A partly written last line
/// (the process was killed mid-write) is dropped.
pub fn load_chain(path: &std::path::Path, use_snapshot: bool, mut on_block: impl FnMut(&State, &Finalized, &BlockBody, &BlockOutput)) -> Result<Option<Loaded>, String> {
    use std::io::{BufRead, Read, Seek};
    let Ok(file) = std::fs::File::open(path) else { return Ok(None) };
    let mut r = std::io::BufReader::with_capacity(8 << 20, file);
    let mut line = String::new();
    if r.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
        return Ok(None);
    }
    let header: serde_json::Value = serde_json::from_str(&line).map_err(|e| format!("bad log header: {e}"))?;
    let genesis = genesis_from_header(&header)?;
    let segment_start = header["snapshot_height"].as_u64().unwrap_or(0);
    let mut valid_len = line.len() as u64;

    // Pick the starting state.
    let snap = match std::fs::File::open(snapshot_path(path)) {
        Ok(f) if use_snapshot || segment_start > 0 => {
            let mut sr = std::io::BufReader::new(f);
            let mut meta = String::new();
            sr.read_line(&mut meta).map_err(|e| e.to_string())?;
            let meta: SnapshotMeta = serde_json::from_str(&meta).map_err(|e| format!("bad snapshot header: {e}"))?;
            Some((meta, sr))
        }
        _ => None,
    };
    let usable = snap.filter(|(m, _)| m.genesis_time_ms == genesis.time_ms && (m.height == segment_start || (m.segment_start == segment_start && m.height > segment_start)));
    let state = match usable {
        Some((meta, mut sr)) => {
            let mut bytes = Vec::new();
            sr.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
            let state: State = postcard::from_bytes(&bytes).map_err(|e| format!("bad snapshot: {e}"))?;
            if state.height != meta.height || state.hash() != meta.state_hash {
                return Err(format!("snapshot at height {} does not match its recorded state hash", meta.height));
            }
            // A snapshot taken mid-segment (the log was not rotated after it): skip to its offset.
            if meta.height > segment_start {
                r.seek(std::io::SeekFrom::Start(meta.log_offset)).map_err(|e| e.to_string())?;
                valid_len = meta.log_offset;
            }
            state
        }
        None if segment_start == 0 => State::genesis(&genesis),
        None => return Err(format!("log segment starts after height {segment_start} but there is no matching snapshot at {}", snapshot_path(path).display())),
    };
    let start_height = state.height;
    let mut state = state;
    let mut last_certificate = None;
    loop {
        line.clear();
        let n = r.read_line(&mut line).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        let Ok(e) = serde_json::from_str::<LogEntry>(&line) else {
            tracing::warn!(at_byte = valid_len, "block log ends in a partly written line; dropping it");
            break;
        };
        valid_len += n as u64;
        if e.height <= state.height {
            continue;
        }
        let f = e.finalized()?;
        let (body, out) = execute(&mut state, &f, None);
        if out.state_hash != e.state_hash {
            return Err(format!("state hash mismatch at height {}: log {:?}, replay {:?}", e.height, e.state_hash, out.state_hash));
        }
        last_certificate = (!f.certificate.is_empty()).then(|| (f.height, f.certificate.clone()));
        on_block(&state, &f, &body, &out);
    }
    Ok(Some(Loaded { header, genesis, state, start_height, segment_start, valid_len, last_certificate }))
}

/// Entries of the block log at `path` from height `from`, at most `limit`.
/// Serves peers that are catching up; only blocks since the last snapshot
/// are in the log.
pub fn read_log(path: &std::path::Path, from: u64, limit: usize) -> std::io::Result<Vec<LogEntry>> {
    use std::io::BufRead;
    let r = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut out = Vec::new();
    for line in r.lines().skip(1) {
        let Ok(e) = serde_json::from_str::<LogEntry>(&line?) else { break };
        if e.height >= from {
            out.push(e);
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}
