//! Restart from the block log: the executor writes the log and state
//! snapshots (rotating the log after each one), and `load_chain` rebuilds the
//! exact same state from them — from genesis, from a snapshot, after the
//! process died mid-write, and after a resumed run appended more blocks.

use hc_consensus::Finalized;
use hc_core::action::{OrderTypeWire, OrderWire};
use hc_core::types::{Hash32, Tif};
use hc_core::{Action, BlockBody, SignedTx, State};
use hc_node::node::{execute, load_chain, run_executor, snapshot_path, LogSink, Shared};
use hc_node::{dev_user, devnet_genesis, DEV_USERS};
use std::path::{Path, PathBuf};

const T0: u64 = 1_760_000_000_000;

/// Block `h`: every dev user crosses a small BTC order with the book, so
/// blocks carry fills, fees and position changes.
fn block(h: u64) -> Finalized {
    let txs: Vec<SignedTx> = DEV_USERS
        .iter()
        .enumerate()
        .map(|(i, u)| {
            let buy = (h + i as u64).is_multiple_of(2);
            let px = if buy { "100100" } else { "99900" };
            let o = OrderWire { a: 0, b: buy, p: px.into(), s: "0.01".into(), r: false, t: OrderTypeWire::limit(Tif::Gtc), c: None };
            SignedTx::sign(&dev_user(u).0, Action::order(vec![o]), T0 + h * 10 + i as u64)
        })
        .collect();
    let body = BlockBody { time_ms: T0 + h * 100, txs, evm_txs: vec![] };
    Finalized { view: h, height: h, proposer: (h % 4) as u32, digest: [0; 32], payload: body.encode(), certificate: vec![] }
}

/// State hashes after each of blocks 1..=n, executed directly.
fn reference(n: u64) -> Vec<Hash32> {
    let mut st = State::genesis(&devnet_genesis(T0));
    (1..=n).map(|h| execute(&mut st, &block(h), None).1.state_hash).collect()
}

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hc-resume-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Runs the executor over blocks `from..=to` on top of `state`, writing to `log`.
fn run(state: State, from: u64, to: u64, log: LogSink) -> State {
    let shared = Shared::new(&devnet_genesis(T0), vec![], Default::default());
    *shared.state.write().unwrap() = state;
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    for h in from..=to {
        tx.send(block(h)).unwrap();
    }
    drop(tx);
    rt.block_on(run_executor(shared.clone(), rx, Some(log)));
    let st = shared.state.read().unwrap().clone();
    st
}

fn load(path: &Path, use_snapshot: bool) -> hc_node::node::Loaded {
    load_chain(path, use_snapshot, |_, _, _, _| {}).expect("loads").expect("log exists")
}

#[test]
fn full_log_replays_from_genesis_or_snapshot() {
    let d = dir("full");
    let log = d.join("blocks.jsonl");
    let genesis = devnet_genesis(T0);
    let st = run(State::genesis(&genesis), 1, 35, LogSink::create(&log, &genesis, 10, false).unwrap());
    let want = reference(35);
    assert_eq!(st.hash(), want[34]);

    let g = load(&log, false);
    assert_eq!((g.start_height, g.state.height, g.state.hash()), (0, 35, want[34]));
    // The snapshot at 30 is mid-segment: loading seeks past the blocks it covers.
    let s = load(&log, true);
    assert_eq!((s.start_height, s.state.height, s.state.hash()), (30, 35, want[34]));
    assert_eq!(s.valid_len, std::fs::metadata(&log).unwrap().len());
}

#[test]
fn rotated_log_resumes_after_crash_and_appends() {
    let d = dir("rotate");
    let log = d.join("blocks.jsonl");
    let genesis = devnet_genesis(T0);
    run(State::genesis(&genesis), 1, 35, LogSink::create(&log, &genesis, 10, true).unwrap());
    let want = reference(50);

    // The log now holds only blocks 31..=35 after a header naming snapshot 30.
    let text = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 6);
    assert!(lines[0].contains("\"snapshot_height\":30"), "{}", lines[0]);
    // Genesis is gone, so even without `use_snapshot` the snapshot is used.
    assert_eq!(load(&log, false).start_height, 30);

    // Killed mid-write: half of block 35's line made it to disk.
    let cut = text.len() - lines[5].len() / 2 - 1;
    std::fs::write(&log, &text[..cut]).unwrap();
    let l = load(&log, true);
    assert_eq!((l.state.height, l.state.hash()), (34, want[33]));
    assert_eq!(l.valid_len as usize, text.len() - lines[5].len() - 1);

    // Resume: the partial line is dropped and blocks 35..=50 are appended
    // (with a snapshot and rotation at 40 and 50 on the way).
    let sink = LogSink::resume(&log, &l, 10, true).unwrap();
    let st = run(l.state, 35, 50, sink);
    assert_eq!(st.hash(), want[49]);
    let r = load(&log, true);
    assert_eq!((r.start_height, r.state.height, r.state.hash()), (50, 50, want[49]));
}

#[test]
fn tampered_snapshot_or_log_is_rejected() {
    let d = dir("tamper");
    let log = d.join("blocks.jsonl");
    let genesis = devnet_genesis(T0);
    run(State::genesis(&genesis), 1, 25, LogSink::create(&log, &genesis, 10, false).unwrap());

    // A log line whose recorded state hash differs from re-execution.
    let text = std::fs::read_to_string(&log).unwrap();
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let mut e: serde_json::Value = serde_json::from_str(&lines[3]).unwrap();
    e["state_hash"] = serde_json::to_value(Hash32([7; 32])).unwrap();
    lines[3] = e.to_string();
    let bad = d.join("bad.jsonl");
    std::fs::write(&bad, lines.join("\n") + "\n").unwrap();
    let err = load_chain(&bad, false, |_, _, _, _| {}).err().expect("rejected");
    assert!(err.contains("state hash mismatch at height 3"), "{err}");

    // A snapshot whose state does not hash to the value in its header.
    let snap = std::fs::read(snapshot_path(&log)).unwrap();
    let nl = snap.iter().position(|b| *b == b'\n').unwrap();
    let mut meta: serde_json::Value = serde_json::from_slice(&snap[..nl]).unwrap();
    meta["state_hash"] = serde_json::to_value(Hash32([9; 32])).unwrap();
    let mut forged = format!("{meta}\n").into_bytes();
    forged.extend_from_slice(&snap[nl + 1..]);
    std::fs::write(snapshot_path(&log), forged).unwrap();
    let err = load_chain(&log, true, |_, _, _, _| {}).err().expect("rejected");
    assert!(err.contains("does not match its recorded state hash"), "{err}");
}

#[test]
fn new_chain_deletes_old_snapshot() {
    let d = dir("fresh");
    let log = d.join("blocks.jsonl");
    let genesis = devnet_genesis(T0);
    run(State::genesis(&genesis), 1, 12, LogSink::create(&log, &genesis, 10, true).unwrap());
    assert!(snapshot_path(&log).exists());
    let sink = LogSink::create(&log, &genesis, 10, true).unwrap();
    assert!(!snapshot_path(&log).exists());
    drop(sink);
    let l = load(&log, true);
    assert_eq!((l.start_height, l.state.height), (0, 0));
}

#[test]
fn candles_survive_a_resume_without_double_counting() {
    use hc_node::history::{candle_path, History};
    let d = dir("candles");
    let log = d.join("blocks.jsonl");
    let genesis = devnet_genesis(T0);
    run(State::genesis(&genesis), 1, 25, LogSink::create(&log, &genesis, 10, true).unwrap());

    // History of an uninterrupted chain, saving candles at every 10th block.
    let mut st = State::genesis(&genesis);
    let mut full = History::default();
    full.candle_file = Some((candle_path(&log), 10));
    for h in 1..=25 {
        let f = block(h);
        let (body, out) = execute(&mut st, &f, None);
        full.record(&st, f.view, f.proposer, &body, &out);
    }
    // Resume: candles saved at 20, then blocks 21..=25 replayed from the log.
    let mut resumed = History::default();
    assert_eq!(resumed.load_candles(&candle_path(&log)), Some(20));
    load_chain(&log, true, |st, f, body, out| resumed.record(st, f.view, f.proposer, body, out)).unwrap();
    let key = (0, 0); // BTC, 1s candles
    let (a, b) = (&full.candles[&key], &resumed.candles[&key]);
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(b) {
        assert_eq!((x.t, x.open, x.close, x.volume, x.n), (y.t, y.open, y.close, y.volume, y.n));
    }
}
