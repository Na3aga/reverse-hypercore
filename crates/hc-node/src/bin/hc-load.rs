//! End-to-end load test against a running devnet.
//!
//! Funds a set of accounts with the faucet, then for each target rate:
//! pre-signs that stage's order flow (so signing does not compete with the
//! node), sends it over WebSocket `post` requests at the target rate, and
//! reports what the chain actually executed (from the `blocks` stream) and the
//! submit-to-result latency.
//!
//! Order flow per account: GTC limits a few ticks from the mid (each with a
//! client order id), IOC orders that cross the spread, and a cancel-by-cloid of
//! the account's order from five orders back, so the book stays bounded.
//!
//!   cargo run --release --bin hc-load -- --rates 25000,50000,100000 --secs 10

use clap::Parser;
use futures::{SinkExt, StreamExt};
use hc_core::action::{dev_key, key_address, CancelByCloidWire, OrderTypeWire, OrderWire, SigningKey};
use hc_core::types::{Cloid, Tif};
use hc_core::{Action, SignedTx};
use rayon::prelude::*;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio_tungstenite::tungstenite::Message;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:8080")]
    api: String,
    #[arg(long, default_value_t = 2_000)]
    accounts: usize,
    /// WebSocket connections used to send.
    #[arg(long, default_value_t = 32)]
    conns: usize,
    /// Target transaction rates (tx/s), one stage each.
    #[arg(long, default_value = "25000,50000,100000")]
    rates: String,
    /// Seconds per stage.
    #[arg(long, default_value_t = 10)]
    secs: u64,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_millis() as u64
}

struct Acct {
    key: SigningKey,
    nonce: u64,
    seq: u64,
    resting: VecDeque<Cloid>,
}

fn cloid(acct: usize, seq: u64) -> Cloid {
    let mut c = [0u8; 16];
    c[..8].copy_from_slice(&(acct as u64).to_be_bytes());
    c[8..].copy_from_slice(&seq.to_be_bytes());
    Cloid(c)
}

fn limit(px: i64, buy: bool, sz: &str, tif: Tif, c: Option<Cloid>) -> OrderWire {
    OrderWire { a: 0, b: buy, p: px.to_string(), s: sz.into(), r: false, t: OrderTypeWire::limit(tif), c }
}

/// One stage of pre-signed requests: (connection, id, JSON text), in send order.
fn build_stage(accts: &mut [Acct], n: usize, first_id: u64, mid: i64, conns: usize, salt: u64) -> Vec<(usize, u64, String)> {
    // Decide the actions sequentially (cheap), then sign in parallel (expensive).
    let mut plan: Vec<(usize, Action, u64)> = Vec::with_capacity(n);
    let mut rng = salt;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    for i in 0..n {
        let a = i % accts.len();
        let acct = &mut accts[a];
        acct.nonce += 1;
        let r = next() % 100;
        let action = if acct.resting.len() >= 5 && r < 30 {
            let c = acct.resting.pop_front().expect("non-empty");
            Action::CancelByCloid { cancels: vec![CancelByCloidWire { asset: 0, cloid: c }] }
        } else if r < 70 {
            acct.seq += 1;
            let c = cloid(a, acct.seq);
            acct.resting.push_back(c);
            let buy = next() % 2 == 0;
            let off = 1 + (next() % 15) as i64;
            Action::order(vec![limit(if buy { mid - off } else { mid + off }, buy, "0.001", Tif::Gtc, Some(c))])
        } else {
            let buy = next() % 2 == 0;
            Action::order(vec![limit(if buy { mid + mid / 500 } else { mid - mid / 500 }, buy, "0.002", Tif::Ioc, None)])
        };
        plan.push((a, action, acct.nonce));
    }
    plan.into_par_iter()
        .enumerate()
        .map(|(i, (a, action, nonce))| {
            let tx = SignedTx::sign(&accts_key(a), action, nonce);
            let id = first_id + i as u64;
            (a % conns, id, json!({"method": "post", "id": id, "request": {"type": "action", "payload": tx}}).to_string())
        })
        .collect()
}

// Keys are derived from labels, so workers can rebuild them without sharing.
fn accts_key(a: usize) -> SigningKey {
    thread_local! { static KEYS: std::cell::RefCell<std::collections::HashMap<usize, SigningKey>> = Default::default(); }
    KEYS.with(|k| k.borrow_mut().entry(a).or_insert_with(|| dev_key(&format!("load-{a}"))).clone())
}

#[derive(Default)]
struct Tally {
    ok: AtomicU64,
    err: AtomicU64,
    lat_us: std::sync::Mutex<Vec<u64>>,
    /// Error reasons (first words) -> count.
    reasons: std::sync::Mutex<std::collections::BTreeMap<String, u64>>,
}

async fn info(api: &str, body: Value) -> anyhow::Result<Value> {
    Ok(reqwest::Client::new().post(format!("http://{api}/info")).json(&body).send().await?.json().await?)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let rates: Vec<usize> = args.rates.split(',').map(|r| r.trim().parse()).collect::<Result<_, _>>()?;
    let base_nonce = now_ms();
    let mut accts: Vec<Acct> =
        (0..args.accounts).map(|a| Acct { key: dev_key(&format!("load-{a}")), nonce: base_nonce, seq: 0, resting: VecDeque::new() }).collect();

    // Connections: sender + reader per socket; replies are matched by id.
    let tally = Arc::new(Tally::default());
    let total: usize = args.accounts + 16 + rates.iter().map(|r| r * args.secs as usize).sum::<usize>();
    let sent_at: Arc<Vec<AtomicU64>> = Arc::new((0..total + args.accounts + 16).map(|_| AtomicU64::new(0)).collect::<Vec<_>>());
    let t_base = Instant::now();
    let mut senders = Vec::new();
    for _ in 0..args.conns {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws", args.api)).await?;
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<String>>();
        senders.push(tx);
        tokio::spawn(async move {
            while let Some(batch) = rx.recv().await {
                for m in batch {
                    if sink.feed(Message::text(m)).await.is_err() {
                        return;
                    }
                }
                if sink.flush().await.is_err() {
                    return;
                }
            }
        });
        let (tally, sent_at) = (tally.clone(), sent_at.clone());
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let Message::Text(t) = msg else { continue };
                let Ok(v) = serde_json::from_str::<Value>(&t) else { continue };
                if v["channel"] != "post" {
                    continue;
                }
                let id = v["data"]["id"].as_u64().unwrap_or(0) as usize;
                let payload = &v["data"]["response"]["payload"];
                let ok = payload["status"] == "ok";
                (if ok { &tally.ok } else { &tally.err }).fetch_add(1, Ordering::Relaxed);
                if !ok {
                    let why: String = payload["response"].as_str().unwrap_or("?").chars().take(40).collect();
                    *tally.reasons.lock().unwrap().entry(why).or_default() += 1;
                }
                if let Some(s) = sent_at.get(id) {
                    let sent = s.load(Ordering::Relaxed);
                    if sent > 0 {
                        tally.lat_us.lock().unwrap().push(t_base.elapsed().as_micros() as u64 - sent);
                    }
                }
            }
        });
    }

    // Chain throughput from the blocks stream: (arrival, height, nTxs).
    let blocks = Arc::new(std::sync::Mutex::new(Vec::<(Instant, u64, u64)>::new()));
    {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws", args.api)).await?;
        let (mut sink, mut stream) = ws.split();
        sink.send(Message::text(json!({"method": "subscribe", "subscription": {"type": "blocks"}}).to_string())).await?;
        let blocks = blocks.clone();
        tokio::spawn(async move {
            let _keep = sink;
            while let Some(Ok(Message::Text(t))) = stream.next().await {
                let Ok(v) = serde_json::from_str::<Value>(&t) else { continue };
                if v["channel"] == "blocks" {
                    blocks.lock().unwrap().push((Instant::now(), v["data"]["height"].as_u64().unwrap_or(0), v["data"]["nTxs"].as_u64().unwrap_or(0)));
                }
            }
        });
    }

    // Fund every account (100k USDC each), then wait for the chain to include it.
    println!("funding {} accounts…", args.accounts);
    let mut id = 1u64;
    let fund: Vec<(usize, u64, String)> = accts
        .iter_mut()
        .enumerate()
        .map(|(a, acct)| {
            acct.nonce += 1;
            let tx = SignedTx::sign(&acct.key, Action::Faucet { usd: "100000".into() }, acct.nonce);
            id += 1;
            (a % args.conns, id, json!({"method": "post", "id": id, "request": {"type": "action", "payload": tx}}).to_string())
        })
        .collect();
    send_all(&senders, fund, &sent_at, t_base);
    let deadline = Instant::now() + Duration::from_secs(60);
    while tally.ok.load(Ordering::Relaxed) + tally.err.load(Ordering::Relaxed) < args.accounts as u64 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!("funded: {} ok, {} errors", tally.ok.load(Ordering::Relaxed), tally.err.load(Ordering::Relaxed));
    let probe = key_address(&accts[0].key);
    let st = info(&args.api, json!({"type": "clearinghouseState", "user": probe})).await?;
    println!("account 0 value: {}", st["marginSummary"]["accountValue"]);

    println!("\n{:>9} | {:>9} | {:>10} | {:>8} | {:>8} | {:>8} | {:>8} | {:>7}", "target", "sent/s", "chain tx/s", "ok", "err", "p50 ms", "p99 ms", "blk/s");
    for (si, &rate) in rates.iter().enumerate() {
        let mids = info(&args.api, json!({"type": "allMids"})).await?;
        let mid: f64 = mids["BTC"].as_str().unwrap_or("0").parse()?;
        let n = rate * args.secs as usize;
        let t = Instant::now();
        let stage = build_stage(&mut accts, n, id + 1, mid.round() as i64, args.conns, 0x9e3779b97f4a7c15 ^ si as u64);
        id += n as u64;
        let sign_s = t.elapsed().as_secs_f64();

        let (ok0, err0) = (tally.ok.load(Ordering::Relaxed), tally.err.load(Ordering::Relaxed));
        tally.lat_us.lock().unwrap().clear();
        tally.reasons.lock().unwrap().clear();
        let b0 = blocks.lock().unwrap().len();
        let start = Instant::now();
        // Pace in 5 ms slices.
        let slice = Duration::from_millis(5);
        let per_slice = (rate as f64 * slice.as_secs_f64()).ceil() as usize;
        let mut it = stage.into_iter().peekable();
        let mut k = 0u32;
        while it.peek().is_some() {
            let batch: Vec<_> = it.by_ref().take(per_slice).collect();
            send_all(&senders, batch, &sent_at, t_base);
            k += 1;
            let due = start + slice * k;
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                tokio::time::sleep(wait).await;
            }
        }
        let send_s = start.elapsed().as_secs_f64();
        // Wait for the replies (bounded).
        let deadline = Instant::now() + Duration::from_secs(40);
        while (tally.ok.load(Ordering::Relaxed) + tally.err.load(Ordering::Relaxed)) - (ok0 + err0) < n as u64 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let end = Instant::now();
        let ok = tally.ok.load(Ordering::Relaxed) - ok0;
        let err = tally.err.load(Ordering::Relaxed) - err0;
        let (chain_rate, blk_rate) = {
            let b = blocks.lock().unwrap();
            let stage_blocks: Vec<_> = b[b0..].iter().filter(|(at, _, _)| *at >= start && *at <= end).collect();
            let txs: u64 = stage_blocks.iter().map(|(_, _, n)| n).sum();
            let span = stage_blocks.last().map_or(1e-9, |l| l.0.duration_since(start).as_secs_f64());
            (txs as f64 / span, stage_blocks.len() as f64 / span)
        };
        let mut lat = std::mem::take(&mut *tally.lat_us.lock().unwrap());
        lat.sort_unstable();
        let pct = |p: f64| lat.get(((lat.len() as f64 * p) as usize).min(lat.len().saturating_sub(1))).map_or(f64::NAN, |v| *v as f64 / 1e3);
        println!(
            "{:>9} | {:>9.0} | {:>10.0} | {:>8} | {:>8} | {:>8.0} | {:>8.0} | {:>7.1}   (pre-signed in {:.1} s)",
            rate,
            n as f64 / send_s,
            chain_rate,
            ok,
            err,
            pct(0.5),
            pct(0.99),
            blk_rate,
            sign_s
        );
        for (why, n) in std::mem::take(&mut *tally.reasons.lock().unwrap()) {
            println!("{:>12} errors: {why}", n);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Ok(())
}

fn send_all(senders: &[tokio::sync::mpsc::UnboundedSender<Vec<String>>], msgs: Vec<(usize, u64, String)>, sent_at: &[AtomicU64], t_base: Instant) {
    let mut per: Vec<Vec<String>> = vec![Vec::new(); senders.len()];
    let now = t_base.elapsed().as_micros() as u64;
    for (c, id, m) in msgs {
        if let Some(s) = sent_at.get(id as usize) {
            s.store(now.max(1), Ordering::Relaxed);
        }
        per[c].push(m);
    }
    for (s, batch) in senders.iter().zip(per) {
        if !batch.is_empty() {
            let _ = s.send(batch);
        }
    }
}
