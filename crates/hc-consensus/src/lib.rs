//! # hc-consensus
//!
//! Application-agnostic BFT consensus over opaque block payloads, built on
//! [Commonware simplex](commonware_consensus::simplex) (ed25519 attributable
//! signatures, round-robin leaders, `n >= 3f + 1`).
//!
//! The application plugs in through [`PayloadSource`] (one instance per
//! validator) and receives an ordered, gap-free stream of [`Finalized`]
//! blocks. Two drivers are provided:
//!
//! - [`run_deterministic`]: all validators on Commonware's deterministic
//!   runtime and simulated network. Fully reproducible for a given seed (and
//!   deterministic `PayloadSource` behavior): virtual time, seeded RNG, seeded
//!   keys, seeded link latency/jitter/loss.
//! - [`spawn_tokio`]: the same cluster on Commonware's tokio runtime (real
//!   time), all validators in one process but talking over Commonware's
//!   *authenticated* `lookup` p2p (encrypted, ed25519-identified TCP on
//!   `127.0.0.1`). Commonware's simulated network cannot run on the tokio
//!   runtime (it binds fake IPs through the runtime's network), hence the
//!   real transport. Validator 0's finalized stream is forwarded to a
//!   `tokio::sync::mpsc` sink.
//!
//! ## Blocks, digests and payload dissemination
//!
//! Simplex orders 32-byte digests only. Each proposal is an *envelope*:
//!
//! ```text
//! view u64 | parent_view u64 | parent_digest [u8;32] | proposer u32 | len u32 | payload
//! ```
//!
//! (big-endian) and its digest is `sha256(envelope)`. This is the digest
//! simplex votes on, the one exposed as [`Finalized::digest`], and the
//! `parent_digest` handed to [`PayloadSource::build`] (all zeros for
//! genesis). Binding view/parent/proposer into the digest prevents a payload
//! from being replayed into another view or onto another parent.
//!
//! Full envelopes travel on a dedicated p2p channel (separate from simplex's
//! vote/certificate/resolver channels):
//!
//! 1. The leader builds the envelope, stores it, returns its digest to simplex,
//!    and, when simplex calls `Relay::broadcast` (`Plan::Propose`), sends the
//!    bytes to all peers. `Plan::Forward` requests from simplex resend it to
//!    lagging peers.
//! 2. A peer asked to `verify` a digest it does not have keeps the request
//!    *pending* and broadcasts a `Request(digest)`; any peer holding the
//!    envelope replies with it. Requests are retried every 250 ms. Envelopes
//!    are self-authenticating (the digest is recomputed on receipt).
//! 3. Once the envelope is present it is checked against the consensus context
//!    (view, parent, leader == proposer) and then by
//!    [`PayloadSource::validate`]; `false` refuses the vote.
//! 4. On a finalization certificate for view `v`, the validator walks parent
//!    links from `v`'s block back to its last delivered block, fetching any
//!    envelope it lacks (e.g. it was offline or packets were dropped), and
//!    then delivers the missing blocks oldest-first. Hence `height` is a
//!    consecutive count starting at 1, and nullified views produce no block.
//!
//! ## Deviations from the original API sketch
//!
//! - [`PayloadSource::build`] takes `parent_digest: [u8; 32]` explicitly (the
//!   sketch's doc comment described it but the signature omitted it).
//! - [`SimConfig`] has extra fields (`offline`, `max_sim_time_ms`,
//!   `block_interval_ms`); use `..SimConfig::default()`.
//! - [`spawn_tokio`] takes the `shutdown` future as an argument and returns a
//!   [`LiveHandle`] whose [`LiveHandle::join`] waits for the runtime thread.
//! - [`LiveConfig`] has no latency/jitter/loss knobs (real TCP is used); it
//!   has `base_port`, `block_interval_ms`, `storage_dir`, `worker_threads`.
//!
//! ## Caveats
//!
//! - [`spawn_tokio`] runs all validators in one process (one network stack
//!   each); [`spawn_tokio_validator`] runs one validator per process, with an
//!   application gossip channel on the same authenticated p2p network.
//! - With `base_port: None`, free ports are probed by binding `127.0.0.1:0`
//!   and releasing them, which is slightly racy.
//! - Leader timeouts are >= 1 s, so a crashed leader costs ~1 s per view it
//!   leads. Payload fetches are broadcast to all peers (fine for small `n`).
//! - No restart/recovery: each run starts from genesis (the tokio mode uses a
//!   fresh storage directory per run).
//! - Validators keep envelopes for `1024` views behind the finalized tip to
//!   serve fetches; peers lagging further than that cannot catch up.

mod actor;
mod block;
mod cluster;

pub use block::MAX_PAYLOAD_BYTES;

/// Most bytes in one gossip message.
pub const MAX_GOSSIP_BYTES: usize = cluster::MAX_GOSSIP_BYTES;

use commonware_runtime::{
    deterministic, tokio as cw_tokio, Clock as _, Runner as _, Supervisor as _,
};
use std::{
    collections::BTreeSet,
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::{Duration, UNIX_EPOCH},
};

/// Supplied by the application (one instance per validator).
pub trait PayloadSource: Send + Sync + 'static {
    /// Called when this validator is leader for `view`. `parent_digest` is the
    /// 32-byte digest of the parent block (zeros for genesis). `now_ms` is the
    /// runtime clock in milliseconds since the Unix epoch (virtual time starting
    /// at 0 under the deterministic runtime). Returns opaque payload bytes (at
    /// most [`MAX_PAYLOAD_BYTES`]; larger payloads are not proposed).
    fn build(&self, view: u64, parent_digest: [u8; 32], now_ms: u64) -> Vec<u8>;

    /// Structural validation of a peer's payload; return `false` to refuse
    /// voting. Must be deterministic across honest validators.
    fn validate(&self, view: u64, payload: &[u8]) -> bool;
}

/// A finalized block as observed by one validator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finalized {
    /// Consensus view in which the block was proposed.
    pub view: u64,
    /// Consecutive count of finalized blocks, starting at 1.
    pub height: u64,
    /// Index of the proposing validator.
    pub proposer: u32,
    /// `sha256` of the block envelope (see crate docs).
    pub digest: [u8; 32],
    /// The application's opaque payload bytes.
    pub payload: Vec<u8>,
}

/// Configuration for [`run_deterministic`].
#[derive(Clone, Debug)]
pub struct SimConfig {
    /// Number of validators (4 tolerates one fault).
    pub n_validators: usize,
    /// Seed for keys, runtime RNG and network randomness.
    pub seed: u64,
    /// Stop once every online validator has finalized this many blocks.
    pub max_finalized: u64,
    /// Mean one-way link latency.
    pub link_latency_ms: u64,
    /// Standard deviation of link latency.
    pub link_jitter_ms: u64,
    /// Probability in `[0, 1]` that a message is delivered.
    pub packet_success_rate: f64,
    /// Validator indices that never start (crashed from genesis).
    pub offline: Vec<usize>,
    /// Virtual-time budget; the run returns whatever was finalized by then.
    pub max_sim_time_ms: u64,
    /// Delay between a leader being asked to propose and building the payload.
    pub block_interval_ms: u64,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            n_validators: 4,
            seed: 0,
            max_finalized: 20,
            link_latency_ms: 50,
            link_jitter_ms: 5,
            packet_success_rate: 1.0,
            offline: Vec::new(),
            max_sim_time_ms: 600_000,
            block_interval_ms: 0,
        }
    }
}

/// Deterministic-runtime simulation. Returns, per validator, the ordered list
/// of finalized blocks (truncated to `max_finalized`; empty for offline
/// validators). Fully deterministic for the same `cfg` and the same
/// `PayloadSource` behavior.
///
/// # Panics
///
/// If `sources.len() != cfg.n_validators` or `packet_success_rate` is not in `[0, 1]`.
pub fn run_deterministic(
    cfg: SimConfig,
    sources: Vec<Arc<dyn PayloadSource>>,
) -> Vec<Vec<Finalized>> {
    let n = cfg.n_validators;
    let outputs: Vec<Arc<Mutex<Vec<Finalized>>>> =
        (0..n).map(|_| Arc::new(Mutex::new(Vec::new()))).collect();
    let sinks: Vec<actor::FinalizedSink> = outputs
        .iter()
        .map(|out| {
            let out = out.clone();
            Arc::new(move |f: Finalized| out.lock().unwrap().push(f)) as actor::FinalizedSink
        })
        .collect();
    let offline: BTreeSet<usize> = cfg.offline.iter().copied().collect();
    let online: Vec<usize> = (0..n).filter(|i| !offline.contains(i)).collect();
    assert_eq!(sources.len(), n, "one PayloadSource per validator");
    let common = cluster::Common {
        seed: cfg.seed,
        sources,
        sinks,
        block_interval: Duration::from_millis(cfg.block_interval_ms),
        expected_delay: Duration::from_millis(cfg.link_latency_ms + cfg.link_jitter_ms),
    };
    let links = cluster::SimLinks {
        latency: Duration::from_millis(cfg.link_latency_ms),
        jitter: Duration::from_millis(cfg.link_jitter_ms),
        success_rate: cfg.packet_success_rate,
        offline,
    };

    let runner = deterministic::Runner::new(deterministic::Config::new().with_seed(cfg.seed));
    let polled = outputs.clone();
    runner.start(|context| async move {
        cluster::start_simulated(context.child("cluster"), common, links).await;
        let deadline = UNIX_EPOCH + Duration::from_millis(cfg.max_sim_time_ms);
        loop {
            let done = online
                .iter()
                .all(|&i| polled[i].lock().unwrap().len() as u64 >= cfg.max_finalized);
            if done || context.current() >= deadline {
                break;
            }
            context.sleep(Duration::from_millis(50)).await;
        }
    });

    outputs
        .into_iter()
        .map(|out| {
            let mut v = std::mem::take(&mut *out.lock().unwrap());
            v.truncate(cfg.max_finalized as usize);
            v
        })
        .collect()
}

/// Configuration for [`spawn_tokio`].
#[derive(Clone, Debug)]
pub struct LiveConfig {
    /// Number of validators.
    pub n_validators: usize,
    /// Seed for validator keys.
    pub seed: u64,
    /// Validator `i` listens on `127.0.0.1:(base_port + i)`. `None` = pick
    /// free ephemeral ports.
    pub base_port: Option<u16>,
    /// Delay between a leader being asked to propose and building the payload
    /// (a crude block-time knob; `0` = as fast as consensus allows).
    pub block_interval_ms: u64,
    /// Storage directory for consensus journals. `None` = a fresh directory
    /// under the system temp dir, removed on shutdown.
    pub storage_dir: Option<PathBuf>,
    /// Tokio worker threads.
    pub worker_threads: usize,
    /// Added to every delivered height, for a chain that resumes after
    /// `height_offset` blocks executed earlier. Consensus itself starts
    /// fresh (views from 0, new journals); only the numbering continues.
    pub height_offset: u64,
}

impl Default for LiveConfig {
    fn default() -> Self {
        Self {
            n_validators: 4,
            seed: 0,
            base_port: None,
            block_interval_ms: 200,
            storage_dir: None,
            worker_threads: 2,
            height_offset: 0,
        }
    }
}

/// Application gossip for a validator running alone in its process
/// ([`spawn_tokio_validator`]): bytes sent on `outbound` go to every peer, and
/// peers' messages are passed to `inbound`. Used to spread transactions
/// between the validators' mempools.
pub struct Gossip {
    pub outbound: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    pub inbound: Arc<dyn Fn(Vec<u8>) + Send + Sync>,
}

/// Placeholder for the validators of other processes; never called.
struct Remote;

impl PayloadSource for Remote {
    fn build(&self, _: u64, _: [u8; 32], _: u64) -> Vec<u8> {
        unreachable!("validator runs in another process")
    }
    fn validate(&self, _: u64, _: &[u8]) -> bool {
        unreachable!("validator runs in another process")
    }
}

/// One validator in this process: validator `index` of `peers.len()`,
/// listening on `peers[index]` and connecting to the others, which run in
/// their own processes (with the same `seed`, so all derive the same keys).
/// Its finalized blocks go to `sink`, numbered from `cfg.height_offset`.
/// `cfg.n_validators` and `cfg.base_port` are ignored.
pub fn spawn_tokio_validator<F>(
    cfg: LiveConfig,
    index: usize,
    peers: Vec<SocketAddr>,
    source: Arc<dyn PayloadSource>,
    sink: tokio::sync::mpsc::UnboundedSender<Finalized>,
    gossip: Gossip,
    shutdown: F,
) -> std::io::Result<LiveHandle>
where
    F: Future<Output = ()> + Send + 'static,
{
    let n = peers.len();
    assert!(index < n, "validator index out of range");
    let offset = cfg.height_offset;
    let sources: Vec<Arc<dyn PayloadSource>> = (0..n).map(|i| if i == index { source.clone() } else { Arc::new(Remote) as Arc<dyn PayloadSource> }).collect();
    let sinks: Vec<actor::FinalizedSink> = (0..n)
        .map(|i| {
            let sink = sink.clone();
            if i == index {
                Arc::new(move |mut f: Finalized| {
                    f.height += offset;
                    let _ = sink.send(f);
                }) as actor::FinalizedSink
            } else {
                Arc::new(|_: Finalized| {}) as actor::FinalizedSink
            }
        })
        .collect();
    let (storage, cleanup) = storage_dir(&cfg, index);
    let common = cluster::Common {
        seed: cfg.seed,
        sources,
        sinks,
        block_interval: Duration::from_millis(cfg.block_interval_ms),
        // Peers may be on other machines: allow for a real network round trip.
        expected_delay: Duration::from_millis(5),
    };
    let runtime_cfg = cw_tokio::Config::new().with_worker_threads(cfg.worker_threads.max(1)).with_storage_directory(storage.clone());
    let thread = std::thread::Builder::new().name("hc-consensus".into()).spawn(move || {
        let runner = cw_tokio::Runner::new(runtime_cfg);
        runner.start(|context| async move {
            cluster::start_lookup_one(context.child("cluster"), common, peers, index, gossip);
            shutdown.await;
        });
        if cleanup {
            let _ = std::fs::remove_dir_all(&storage);
        }
    })?;
    Ok(LiveHandle { thread })
}

/// Consensus journal directory: the configured one, or a fresh temporary
/// one that is removed on shutdown.
fn storage_dir(cfg: &LiveConfig, tag: usize) -> (PathBuf, bool) {
    match cfg.storage_dir.clone() {
        Some(dir) => (dir, false),
        None => {
            let nanos = std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
            (std::env::temp_dir().join(format!("hc-consensus-{}-{}-{tag}-{nanos}", std::process::id(), cfg.seed)), true)
        }
    }
}

/// Handle to a cluster started by [`spawn_tokio`].
pub struct LiveHandle {
    thread: JoinHandle<()>,
}

impl LiveHandle {
    /// Wait for the runtime thread to exit (after `shutdown` resolved).
    pub fn join(self) -> std::thread::Result<()> {
        self.thread.join()
    }

    /// Whether the runtime thread has exited.
    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }
}

/// Live mode: run all validators on Commonware's tokio runtime (on a dedicated
/// OS thread), connected by authenticated `lookup` p2p over localhost TCP.
/// Every block finalized by
/// validator 0 is sent to `sink` exactly once, in height order. Runs until
/// `shutdown` resolves (dropping the sink's receiver is *not* a stop signal).
///
/// # Errors
///
/// If free ports cannot be found or the OS thread cannot be spawned.
pub fn spawn_tokio<F>(
    cfg: LiveConfig,
    sources: Vec<Arc<dyn PayloadSource>>,
    sink: tokio::sync::mpsc::UnboundedSender<Finalized>,
    shutdown: F,
) -> std::io::Result<LiveHandle>
where
    F: Future<Output = ()> + Send + 'static,
{
    let n = cfg.n_validators;
    assert!(n > 0, "need at least one validator");
    let offset = cfg.height_offset;
    let sinks: Vec<actor::FinalizedSink> = (0..n)
        .map(|i| {
            if i == 0 {
                let sink = sink.clone();
                Arc::new(move |mut f: Finalized| {
                    f.height += offset;
                    let _ = sink.send(f);
                }) as actor::FinalizedSink
            } else {
                Arc::new(|_: Finalized| {}) as actor::FinalizedSink
            }
        })
        .collect();
    let (storage, cleanup) = storage_dir(&cfg, 0);
    assert_eq!(sources.len(), n, "one PayloadSource per validator");
    let addrs = listen_addrs(n, cfg.base_port)?;
    let common = cluster::Common {
        seed: cfg.seed,
        sources,
        sinks,
        block_interval: Duration::from_millis(cfg.block_interval_ms),
        expected_delay: Duration::from_millis(1),
    };
    let runtime_cfg = cw_tokio::Config::new()
        .with_worker_threads(cfg.worker_threads.max(1))
        .with_storage_directory(storage.clone());

    let thread = std::thread::Builder::new()
        .name("hc-consensus".into())
        .spawn(move || {
            let runner = cw_tokio::Runner::new(runtime_cfg);
            runner.start(|context| async move {
                cluster::start_lookup(context.child("cluster"), common, addrs);
                shutdown.await;
            });
            if cleanup {
                let _ = std::fs::remove_dir_all(&storage);
            }
        })?;
    Ok(LiveHandle { thread })
}

/// Listen addresses on localhost: `base_port + i`, or probed free ports.
fn listen_addrs(n: usize, base_port: Option<u16>) -> std::io::Result<Vec<SocketAddr>> {
    let localhost = IpAddr::V4(Ipv4Addr::LOCALHOST);
    match base_port {
        Some(base) => (0..n)
            .map(|i| {
                let port = u16::try_from(i)
                    .ok()
                    .and_then(|i| base.checked_add(i))
                    .ok_or_else(|| std::io::Error::other("port overflow"))?;
                Ok(SocketAddr::new(localhost, port))
            })
            .collect(),
        None => {
            // Hold all probes open until every port is chosen so they are distinct.
            let probes = (0..n)
                .map(|_| std::net::TcpListener::bind(SocketAddr::new(localhost, 0)))
                .collect::<std::io::Result<Vec<_>>>()?;
            probes.iter().map(|l| l.local_addr()).collect()
        }
    }
}
