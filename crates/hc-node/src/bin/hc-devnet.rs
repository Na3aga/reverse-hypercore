//! Runs a 4-validator devnet: Commonware simplex consensus over localhost TCP,
//! one executor, validator oracle feeders (live exchange prices by default),
//! the market simulator, and the WebSocket/HTTP API.

use clap::Parser;
use hc_node::feeder::{oracle_tx, px_string, PriceWalk};
use hc_node::price_feed::{LiveFeed, VENUES};
use hc_node::node::{run_executor, Shared};
use hc_node::{devnet_genesis_n, make_sources, validator_key, N_VALIDATORS};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Parser)]
struct Args {
    /// API listen address.
    #[arg(long, default_value = "127.0.0.1:8080")]
    api: SocketAddr,
    /// Append-only block log (JSON lines) for replay.
    #[arg(long, default_value = "devnet-blocks.jsonl")]
    log: PathBuf,
    /// Consensus seed (validator keys).
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Target delay before a leader builds its block (~105 ms blocks at 20).
    #[arg(long, default_value_t = 20)]
    block_interval_ms: u64,
    /// Oracle update period per validator.
    #[arg(long, default_value_t = 3000)]
    oracle_ms: u64,
    /// Oracle source: `live` (spot prices from public exchange APIs) or
    /// `synthetic` (a seeded random walk; no network needed).
    #[arg(long, default_value = "live")]
    oracle: String,
    /// Simulated market activity: about this many trades per block from
    /// background market makers and takers (0 turns the simulator off).
    #[arg(long, default_value_t = 100)]
    sim_trades_per_block: usize,
    /// Number of validators (all in this process). BFT needs n ≥ 3f + 1 to
    /// tolerate f faults: 4 tolerates 1, 7 tolerates 2, 22 tolerates 7.
    #[arg(long, default_value_t = N_VALIDATORS)]
    validators: usize,
    /// Worker threads for the consensus runtime (all validators share it).
    /// Defaults to half the cores.
    #[arg(long)]
    consensus_threads: Option<usize>,
    /// Most transactions a leader puts in one block.
    #[arg(long, default_value_t = 20_000)]
    max_block_txs: usize,
    /// Most bytes of transactions a leader puts in one block (each signed
    /// single order is ~90 bytes, so the default 4 MiB fits ~45k).
    #[arg(long, default_value_t = hc_node::mempool::DEFAULT_MAX_BLOCK_BYTES)]
    max_block_bytes: usize,
    /// Genesis time (ms); defaults to now. Pass the same value to hc-replay.
    #[arg(long)]
    genesis_time_ms: Option<u64>,
    /// Continue the chain in `--log` instead of starting a new one: load the
    /// latest state snapshot, re-execute the blocks logged after it (checking
    /// every state hash), then append new blocks. Starts a new chain if the
    /// log is missing or empty.
    #[arg(long)]
    resume: bool,
    /// Snapshot the state every this many blocks (0 = never), next to the
    /// log as `<log>.snapshot`. Resuming replays only blocks after it.
    #[arg(long, default_value_t = 2_000)]
    snapshot_blocks: u64,
    /// Keep the whole block log from genesis. By default, each snapshot
    /// starts a new log segment and the previous one is deleted, so the log
    /// only holds blocks since the last snapshot (`hc-replay` then replays
    /// from that snapshot).
    #[arg(long)]
    full_log: bool,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("clock after epoch").as_millis() as u64
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let mut args = Args::parse();
    let candles = hc_node::history::candle_path(&args.log);
    let mut history = hc_node::history::History::default();
    history.candle_file = Some((candles.clone(), args.snapshot_blocks));
    let resumed = if args.resume {
        let t0 = std::time::Instant::now();
        if let Some(h) = history.load_candles(&candles) {
            tracing::info!(height = h, "resuming: loaded saved candles");
        }
        let loaded = hc_node::node::load_chain(&args.log, true, |st, f, body, out| {
            history.record(st, f.view, f.proposer, body, out);
            if f.height % 1_000 == 0 {
                tracing::info!(height = f.height, "resuming: re-executing logged blocks");
            }
        })
        .map_err(|e| anyhow::anyhow!("cannot resume from {}: {e}", args.log.display()))?;
        if let Some(l) = &loaded {
            tracing::info!(snapshot = l.start_height, height = l.state.height, secs = format!("{:.1}", t0.elapsed().as_secs_f64()), "resumed chain");
        }
        loaded
    } else {
        None
    };
    if let Some(r) = &resumed {
        if r.genesis.validators.len() != args.validators {
            tracing::warn!(log = r.genesis.validators.len(), arg = args.validators, "--validators differs from the resumed chain; using the chain's");
            args.validators = r.genesis.validators.len();
        }
    }
    let genesis_time = args.genesis_time_ms.unwrap_or_else(now_ms);
    let mut genesis = match &resumed {
        Some(r) => r.genesis.clone(),
        None => devnet_genesis_n(genesis_time, args.validators),
    };

    // With the live oracle, a new chain starts at the current spot prices.
    let coins: Vec<String> = genesis.assets.iter().map(|a| a.name.clone()).collect();
    let feed = match args.oracle.as_str() {
        "live" => {
            let feed = LiveFeed::new(coins);
            let n = feed.poll().await;
            match feed.vote(Duration::from_secs(10)) {
                Some(pxs) => {
                    tracing::info!(quotes = n, venues = VENUES.len(), pxs = ?pxs.iter().map(|p| px_string(*p)).collect::<Vec<_>>(), "live oracle: prices from exchanges");
                    if resumed.is_none() {
                        genesis.oracle_pxs = pxs;
                    }
                    Some(feed)
                }
                None => {
                    tracing::warn!("live oracle: no exchange answered; using the synthetic price walk");
                    None
                }
            }
        }
        "synthetic" => None,
        other => anyhow::bail!("unknown --oracle {other}; use live or synthetic"),
    };
    let signers = std::sync::Arc::new(hc_node::sigcache::SignerCache::default());
    let (mempools, sources) = make_sources(args.validators, 0, args.max_block_txs, args.max_block_bytes.min(hc_consensus::MAX_PAYLOAD_BYTES), signers.clone());
    let shared = Shared::new(&genesis, mempools, signers);

    let (height_offset, log) = match resumed {
        Some(r) => {
            let log = hc_node::node::LogSink::resume(&args.log, &r, args.snapshot_blocks, !args.full_log)?;
            let height = r.state.height;
            *shared.state.write().expect("state lock") = r.state;
            *shared.history.write().expect("history lock") = history;
            tracing::info!(height, log = %args.log.display(), "continuing devnet");
            (height, log)
        }
        None => {
            let log = hc_node::node::LogSink::create(&args.log, &genesis, args.snapshot_blocks, !args.full_log)?;
            *shared.history.write().expect("history lock") = history;
            tracing::info!(genesis_time, log = %args.log.display(), "starting devnet");
            (0, log)
        }
    };

    let (fin_tx, fin_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let cfg = hc_consensus::LiveConfig {
        n_validators: args.validators,
        seed: args.seed,
        block_interval_ms: args.block_interval_ms,
        worker_threads: args.consensus_threads.unwrap_or((cores / 2).max(2)),
        height_offset,
        ..Default::default()
    };
    tracing::info!(validators = cfg.n_validators, consensus_threads = cfg.worker_threads, "consensus config");
    let consensus = hc_consensus::spawn_tokio(cfg, sources, fin_tx, async move {
        let _ = stop_rx.await;
    })?;
    let fin_rx = hc_node::node::spawn_finalizer(shared.clone(), fin_rx);
    tokio::spawn(run_executor(shared.clone(), fin_rx, Some(log)));

    // Oracle feeders: every validator votes each period.
    if let Some(feed) = &feed {
        tokio::spawn(feed.clone().run(Duration::from_millis(args.oracle_ms)));
    }
    for v in 0..args.validators {
        let shared = shared.clone();
        let key = validator_key(v);
        let start = genesis.oracle_pxs.clone();
        let period = args.oracle_ms;
        let feed = feed.clone();
        tokio::spawn(async move {
            let mut walk = PriceWalk::new(42, &start, 15);
            let mut tick = tokio::time::interval(Duration::from_millis(period));
            loop {
                tick.tick().await;
                let pxs = match &feed {
                    // A stale feed means no vote; the oracle holds its last price.
                    Some(feed) => match feed.vote(Duration::from_millis(period * 3)) {
                        Some(pxs) => pxs,
                        None => continue,
                    },
                    None => {
                        walk.step();
                        walk.validator_prices(v)
                    }
                };
                shared.submit(oracle_tx(&key, &pxs, now_ms()));
            }
        });
    }

    tokio::spawn(hc_node::sim::run_market_sim(
        shared.clone(),
        hc_node::sim::SimConfig { trades_per_block: args.sim_trades_per_block, ..Default::default() },
    ));

    let listener = tokio::net::TcpListener::bind(args.api).await?;
    tracing::info!(api = %args.api, "API listening: POST /info, POST /exchange, GET /ws");
    tokio::select! {
        r = axum::serve(listener, hc_node::api::router(shared.clone())) => r?,
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }
    let _ = stop_tx.send(());
    let _ = tokio::task::spawn_blocking(move || consensus.join()).await;
    Ok(())
}
