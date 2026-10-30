//! One validator per process. Each process runs its own consensus validator,
//! mempool, executor, state, block log and API; validators find each other
//! over authenticated p2p and spread transactions over its gossip channel.
//!
//! ```text
//! hc-validator genesis --out genesis.json            # once, shared by all nodes
//! hc-validator run --index 0 --genesis genesis.json --peers 127.0.0.1:9000,127.0.0.1:9001,... --api 127.0.0.1:8080
//! ```
//! `scripts/multi-process.sh` starts four of them on one machine.

use clap::{Parser, Subcommand};
use hc_node::feeder::{oracle_tx, PriceWalk};
use hc_node::node::{run_executor, LogSink, Shared};
use hc_node::price_feed::LiveFeed;
use hc_node::validator_key;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Writes the genesis every node starts from: time, validator count and
    /// starting oracle prices (live spot prices, or the defaults offline).
    Genesis {
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = hc_node::N_VALIDATORS)]
        validators: usize,
        /// `live` or `synthetic`.
        #[arg(long, default_value = "live")]
        oracle: String,
    },
    /// Runs validator `index`.
    Run(RunArgs),
}

#[derive(Parser)]
struct RunArgs {
    /// This validator's index (0-based) in `--peers`.
    #[arg(long)]
    index: usize,
    /// Genesis file from `hc-validator genesis` (the same for every node).
    #[arg(long)]
    genesis: PathBuf,
    /// Every validator's p2p address, in index order (comma-separated).
    #[arg(long, value_delimiter = ',')]
    peers: Vec<SocketAddr>,
    /// Every validator's API address, in index order. A resuming node that is
    /// behind downloads the finalized blocks it missed from them.
    #[arg(long, value_delimiter = ',')]
    peer_apis: Vec<SocketAddr>,
    /// API listen address.
    #[arg(long)]
    api: SocketAddr,
    /// This node's block log; the state snapshot goes next to it.
    #[arg(long)]
    log: PathBuf,
    /// Consensus seed (validator keys); the same on every node.
    #[arg(long, default_value_t = 1)]
    seed: u64,
    #[arg(long, default_value_t = 20)]
    block_interval_ms: u64,
    #[arg(long, default_value_t = 20_000)]
    max_block_txs: usize,
    #[arg(long, default_value_t = hc_node::mempool::DEFAULT_MAX_BLOCK_BYTES)]
    max_block_bytes: usize,
    /// This validator's oracle votes: `live` exchange prices or a `synthetic` walk.
    #[arg(long, default_value = "live")]
    oracle: String,
    #[arg(long, default_value_t = 3000)]
    oracle_ms: u64,
    /// Simulated market activity sent through this node (0 = none).
    #[arg(long, default_value_t = 0)]
    sim_trades_per_block: usize,
    /// Continue this node's chain from its log and snapshot. All nodes must
    /// restart together, each with `--resume`.
    #[arg(long)]
    resume: bool,
    /// With --resume: join a chain whose other validators kept running. The
    /// node catches up from its peers, then starts consensus from the
    /// finalization certificate of the last block instead of genesis.
    #[arg(long, requires = "resume")]
    rejoin: bool,
    #[arg(long, default_value_t = 2_000)]
    snapshot_blocks: u64,
    #[arg(long, default_value_t = 2)]
    consensus_threads: usize,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("clock after epoch").as_millis() as u64
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    match Args::parse().cmd {
        Cmd::Genesis { out, validators, oracle } => {
            let mut genesis = hc_node::devnet_genesis_n(now_ms(), validators);
            if oracle == "live" {
                let feed = LiveFeed::new(genesis.assets.iter().map(|a| a.name.clone()).collect());
                feed.poll().await;
                match feed.vote(Duration::from_secs(10)) {
                    Some(pxs) => genesis.oracle_pxs = pxs,
                    None => tracing::warn!("no exchange answered; using default prices"),
                }
            }
            std::fs::write(&out, format!("{}\n", hc_node::node::log_header(&genesis)))?;
            tracing::info!(out = %out.display(), validators, "wrote genesis");
            Ok(())
        }
        Cmd::Run(args) => run(args).await,
    }
}

async fn run(args: RunArgs) -> anyhow::Result<()> {
    let header: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&args.genesis)?)?;
    let genesis = hc_node::node::genesis_from_header(&header).map_err(anyhow::Error::msg)?;
    let n = genesis.validators.len();
    anyhow::ensure!(args.peers.len() == n, "--peers lists {} addresses but genesis has {n} validators", args.peers.len());
    anyhow::ensure!(args.index < n, "--index {} out of range", args.index);

    // This node's own chain: resumed from its log, or new.
    let candles = hc_node::history::candle_path(&args.log);
    let mut history = hc_node::history::History::default();
    history.candle_file = Some((candles.clone(), args.snapshot_blocks));
    let resumed = if args.resume {
        history.load_candles(&candles);
        hc_node::node::load_chain(&args.log, true, |st, f, body, out| history.record(st, f.view, f.proposer, body, out))
            .map_err(|e| anyhow::anyhow!("cannot resume from {}: {e}", args.log.display()))?
    } else {
        None
    };
    if let Some(r) = &resumed {
        anyhow::ensure!(r.genesis == genesis, "{} belongs to a different genesis", args.log.display());
    }

    let signers = Arc::new(hc_node::sigcache::SignerCache::default());
    let mempool = hc_node::mempool::Mempool::with_limits(args.max_block_txs, args.max_block_bytes);
    let source = Arc::new(hc_node::mempool::ValidatorSource {
        mempool: mempool.clone(),
        time_offset_ms: 0,
        max_payload_txs: args.max_block_txs,
        max_payload_bytes: args.max_block_bytes,
        signers: signers.clone(),
        checked: None,
    });
    let shared = Shared::new(&genesis, vec![mempool], signers);
    *shared.history.write().expect("history lock") = history;
    shared.log_path.set(args.log.clone()).expect("set once");

    // The API starts first, so peers that are behind can fetch blocks from us.
    let listener = tokio::net::TcpListener::bind(args.api).await?;
    let server = tokio::spawn(std::future::IntoFuture::into_future(axum::serve(listener, hc_node::api::router(shared.clone()))));

    let mut floor = None;
    let (height_offset, log) = match resumed {
        Some(mut r) => {
            // Blocks finalized while this node was down: fetch, re-execute, check.
            sync_from_peers(&shared, &mut r, &args).await?;
            if args.rejoin {
                // Consensus starts at the last block this node has; it must carry a certificate.
                match r.last_certificate.take() {
                    Some((h, cert)) if h == r.state.height => floor = Some(cert),
                    _ => anyhow::bail!("no finalization certificate for block {}; cannot rejoin (are the peers running?)", r.state.height),
                }
            }
            let log = LogSink::resume(&args.log, &r, args.snapshot_blocks, true)?;
            let h = r.state.height;
            *shared.state.write().expect("state lock") = r.state;
            tracing::info!(height = h, "resumed chain");
            (h, log)
        }
        None => (0, LogSink::create(&args.log, &genesis, args.snapshot_blocks, true)?),
    };

    // Consensus: this validator only; transactions gossip to the others.
    let gossip = hc_node::gossip::start(shared.clone());
    let (fin_tx, fin_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let cfg = hc_consensus::LiveConfig { seed: args.seed, block_interval_ms: args.block_interval_ms, worker_threads: args.consensus_threads, height_offset, floor, ..Default::default() };
    let consensus = hc_consensus::spawn_tokio_validator(cfg, args.index, args.peers.clone(), source, fin_tx, gossip, async move {
        let _ = stop_rx.await;
    })?;
    let fin_rx = hc_node::node::spawn_finalizer(shared.clone(), fin_rx);
    tokio::spawn(run_executor(shared.clone(), fin_rx, Some(log)));

    // This validator's oracle votes.
    let feed = match args.oracle.as_str() {
        "live" => {
            let feed = LiveFeed::new(genesis.assets.iter().map(|a| a.name.clone()).collect());
            feed.poll().await;
            tokio::spawn(feed.clone().run(Duration::from_millis(args.oracle_ms)));
            Some(feed)
        }
        _ => None,
    };
    {
        let (shared, key, start, period, index) = (shared.clone(), validator_key(args.index), genesis.oracle_pxs.clone(), args.oracle_ms, args.index);
        tokio::spawn(async move {
            let mut walk = PriceWalk::new(42, &start, 15);
            let mut tick = tokio::time::interval(Duration::from_millis(period));
            loop {
                tick.tick().await;
                let pxs = match &feed {
                    Some(feed) => match feed.vote(Duration::from_millis(period * 3)) {
                        Some(pxs) => pxs,
                        None => continue,
                    },
                    None => {
                        walk.step();
                        walk.validator_prices(index)
                    }
                };
                shared.submit(oracle_tx(&key, &pxs, now_ms()));
            }
        });
    }
    if args.sim_trades_per_block > 0 {
        tokio::spawn(hc_node::sim::run_market_sim(shared.clone(), hc_node::sim::SimConfig { trades_per_block: args.sim_trades_per_block, ..Default::default() }));
    }

    tracing::info!(index = args.index, api = %args.api, p2p = %args.peers[args.index], validators = n, "validator running");
    tokio::select! {
        r = server => r??,
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
        _ = wait_term() => tracing::info!("terminated"),
    }
    let _ = stop_tx.send(());
    let _ = tokio::task::spawn_blocking(move || consensus.join()).await;
    Ok(())
}

/// Catches up a resumed node: asks every peer for its latest height, downloads
/// the finalized blocks this node lacks from a peer that has them, and
/// re-executes each one, checking the state hash the peer logged. Then at
/// least f other validators must report the same state hash at this node's
/// height, so a single faulty peer cannot feed it a different chain.
async fn sync_from_peers(shared: &Shared, chain: &mut hc_node::node::Loaded, args: &RunArgs) -> anyhow::Result<()> {
    use std::io::Write;
    let peers: Vec<(usize, SocketAddr)> = args.peer_apis.iter().copied().enumerate().filter(|(i, _)| *i != args.index).collect();
    if peers.is_empty() {
        return Ok(());
    }
    let n = args.peers.len();
    let f = (n - 1) / 3;
    let client = reqwest::Client::new();
    let info = |api: SocketAddr, body: serde_json::Value| {
        let client = client.clone();
        async move {
            let r = client.post(format!("http://{api}/info")).json(&body).timeout(Duration::from_secs(5)).send().await.ok()?;
            r.json::<serde_json::Value>().await.ok()
        }
    };
    // Peers restart at about the same time: give them a few seconds to answer.
    let mut heights: Vec<(usize, SocketAddr, u64)> = Vec::new();
    for _ in 0..20 {
        heights.clear();
        for &(i, api) in &peers {
            if let Some(h) = info(api, serde_json::json!({"type": "stateHash"})).await.and_then(|v| v["latest"].as_u64()) {
                heights.push((i, api, h));
            }
        }
        if heights.len() == peers.len() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let file = std::fs::OpenOptions::new().write(true).open(&args.log)?;
    file.set_len(chain.valid_len)?;
    let mut file = std::fs::OpenOptions::new().append(true).open(&args.log)?;
    while let Some(&(i, api, _)) = heights.iter().filter(|(_, _, h)| *h > chain.state.height).max_by_key(|(_, _, h)| *h) {
        let from = chain.state.height + 1;
        let got = info(api, serde_json::json!({"type": "blockLog", "from": from})).await;
        let entries: Vec<hc_node::node::LogEntry> = got.and_then(|v| serde_json::from_value(v["entries"].clone()).ok()).unwrap_or_default();
        if entries.first().map(|e| e.height) != Some(from) {
            anyhow::bail!("node {i} cannot serve blocks from height {from} (its log starts after its last snapshot); restart this node's chain");
        }
        for e in entries {
            let fin = e.finalized().map_err(|err| anyhow::anyhow!("bad block from node {i}: {err}"))?;
            let (body, out) = hc_node::node::execute(&mut chain.state, &fin, None);
            anyhow::ensure!(out.state_hash == e.state_hash, "block {} from node {i} executes to a different state hash", e.height);
            shared.history.write().expect("history lock").record(&chain.state, e.view, e.proposer, &body, &out);
            let line = format!("{}\n", serde_json::to_string(&e)?);
            file.write_all(line.as_bytes())?;
            chain.valid_len += line.len() as u64;
            if !fin.certificate.is_empty() {
                chain.last_certificate = Some((fin.height, fin.certificate.clone()));
            }
        }
        tracing::info!(height = chain.state.height, from = i, "caught up from peer");
    }
    // Enough peers must agree with where this node ended up.
    let mine = chain.state.hash();
    let mut agree = 0;
    for &(_, api, _) in &heights {
        let theirs = info(api, serde_json::json!({"type": "stateHash", "height": chain.state.height})).await.and_then(|v| serde_json::from_value::<hc_core::types::Hash32>(v["stateHash"].clone()).ok());
        agree += usize::from(theirs == Some(mine));
    }
    anyhow::ensure!(agree >= f, "only {agree} peers report this node's state hash at height {}; need {f}", chain.state.height);
    tracing::info!(height = chain.state.height, peers_agreeing = agree, "in sync with peers");
    Ok(())
}

async fn wait_term() {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut s) => {
            s.recv().await;
        }
        Err(_) => std::future::pending().await,
    }
}
