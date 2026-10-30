//! Background market simulator for the devnet.
//!
//! Real signed transactions from ordinary accounts, submitted through the same
//! mempool path as API traffic, so everything it does is in the block log and
//! replays like any other activity:
//! - **Market makers** (`mm-0`..) keep a ladder of GTC quotes around a fair
//!   price. A refresh posts a new ladder and cancels the one before the
//!   previous ladder, so the book never empties: a block applies cancels
//!   before new orders, and cancel-then-repost would leave a gap for takers
//!   to sweep. The next refresh waits until the previous one is finalized.
//!   Quotes lean against the maker's inventory.
//! - **Takers** (`taker-0`..) send IOC orders that cross the spread, about
//!   `trades_per_block` per block, leaning against their own positions so
//!   inventories stay bounded.
//! - Prices come from the book itself. Makers centre their quotes on the
//!   book's current mid and last trade, so when takers sweep a side the next
//!   ladder is quoted from where the sweep left the price. Order-flow
//!   imbalance therefore moves the price.
//! - The oracle (external spot prices) acts only as an arbitrage anchor: each
//!   refresh closes a small share of the gap between book and oracle. The
//!   gap that remains is the premium that funding charges for.
//!
//! Accounts are funded with the faucet before trading starts and topped up
//! when their account value runs low.

use crate::node::Shared;
use hc_core::action::{dev_key, key_address, CancelWire, OrderTypeWire, OrderWire, SigningKey};
use hc_core::fixed::{format_decimal, round_px, ntl, PX_SCALE};
use hc_core::types::{Address, Hash32, Tif};
use hc_core::{Action, BlockOutput, SignedTx, State};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use std::collections::BTreeSet;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct SimConfig {
    /// Target number of trades (taker IOC orders) per block. 0 disables the simulator.
    pub trades_per_block: usize,
    pub seed: u64,
    pub makers: usize,
    pub takers: usize,
    /// Minimum blocks between maker refreshes.
    pub refresh_blocks: u64,
}

impl Default for SimConfig {
    fn default() -> SimConfig {
        SimConfig { trades_per_block: 100, seed: 7, makers: 4, takers: 20, refresh_blocks: 3 }
    }
}

/// Per-asset trading style: ladder spacing and order sizes in coins.
struct Style {
    half_spread_bps: f64,
    step_bps: f64,
    levels: usize,
    maker_sz: (f64, f64),
    taker_sz: (f64, f64),
    /// Share of taker orders on this asset.
    weight: f64,
    /// Inventory (notional, USD) at which accounts lean fully against their position.
    inventory_limit: f64,
}

fn style(name: &str) -> Style {
    match name {
        "BTC" => Style { half_spread_bps: 0.6, step_bps: 1.5, levels: 14, maker_sz: (0.05, 0.4), taker_sz: (0.001, 0.03), weight: 0.65, inventory_limit: 120_000.0 },
        _ => Style { half_spread_bps: 0.8, step_bps: 2.0, levels: 14, maker_sz: (1.0, 8.0), taker_sz: (0.02, 0.6), weight: 0.35, inventory_limit: 80_000.0 },
    }
}

struct Account {
    key: SigningKey,
    addr: Address,
    last_nonce: u64,
}

impl Account {
    fn new(label: &str) -> Account {
        let key = dev_key(label);
        Account { addr: key_address(&key), key, last_nonce: 0 }
    }

    /// Nonces follow block time, so the simulator is a deterministic function
    /// of the blocks it sees.
    fn sign(&mut self, action: Action, time_ms: u64) -> SignedTx {
        let n = time_ms.max(self.last_nonce + 1);
        self.last_nonce = n;
        SignedTx::sign(&self.key, action, n)
    }
}

/// Account value (USD) and signed position notional per asset.
fn account_view(st: &State, a: Address) -> (f64, Vec<f64>) {
    let Some(u) = st.users.get(&a) else { return (0.0, vec![0.0; st.metas.len()]) };
    let av = st.with_margin_ctx(|ctx| u.summary(ctx).account_value.0) as f64 / 1e6;
    let pos = (0..st.metas.len())
        .map(|i| {
            u.positions.get(&(i as u32)).map_or(0.0, |p| ntl(st.assets[i].prices.mark_px, p.szi, st.metas[i].sz_decimals).0 as f64 / 1e6)
        })
        .collect();
    (av, pos)
}

fn decimal(v: f64, decimals: u32) -> String {
    let scaled = (v * 10f64.powi(decimals as i32)).round().max(1.0) as i128;
    format_decimal(scaled, decimals)
}

fn price(px_usd: f64, sz_decimals: u32, round_up: bool) -> String {
    let p = round_px((px_usd * PX_SCALE as f64) as i128, sz_decimals, round_up);
    format_decimal(p.0 as i128, 6)
}

fn order(a: u32, is_buy: bool, px: String, sz: String, tif: Tif) -> OrderWire {
    OrderWire { a, b: is_buy, p: px, s: sz, r: false, t: OrderTypeWire::limit(tif), c: None }
}

/// Share of the book-to-oracle gap that each fair-price update closes
/// (arbitrage against the external spot market).
const ORACLE_PULL: f64 = 0.05;
/// Taker buy-probability shift per basis point of book-over-oracle premium:
/// takers sell a rich book and buy a cheap one.
const ARB_LEAN_PER_BP: f64 = 0.02;
/// Beyond this distance from the oracle the book is treated as broken (empty
/// side, genesis or a big oracle jump) and quotes restart at the oracle.
const MAX_GAP: f64 = 0.03;

/// The price makers quote around: the book's mid blended with the last trade,
/// nudged toward the oracle.
fn fair_price(st: &State, a: usize) -> f64 {
    let usd = |p: hc_core::fixed::Px| p.0 as f64 / 1e6;
    let oracle = usd(st.assets[a].prices.oracle_px);
    let book = &st.books[a];
    let mid = match (book.best_bid(), book.best_ask()) {
        (Some(b), Some(k)) => Some((usd(b) + usd(k)) / 2.0),
        _ => None,
    };
    let last = st.assets[a].prices.last_trade_px.map(usd);
    let local = match (mid, last) {
        (Some(m), Some(l)) => 0.5 * m + 0.5 * l,
        (Some(m), None) => m,
        (None, Some(l)) => l,
        (None, None) => return oracle,
    };
    if (local / oracle - 1.0).abs() > MAX_GAP {
        return oracle;
    }
    local + ORACLE_PULL * (oracle - local)
}

pub struct MarketSim {
    cfg: SimConfig,
    rng: ChaCha8Rng,
    makers: Vec<Account>,
    takers: Vec<Account>,
    maker_pending: BTreeSet<Hash32>,
    taker_pending: BTreeSet<Hash32>,
    funding_pending: BTreeSet<Hash32>,
    last_refresh: u64,
    /// `next_oid` before the previous refresh: older maker orders get cancelled.
    stale_below: u64,
    prev_mark: u64,
    /// Height at which a pending transaction last finalized; used to recover
    /// if a transaction is never included.
    last_progress: u64,
    funded: bool,
}

impl MarketSim {
    pub fn new(cfg: SimConfig) -> MarketSim {
        MarketSim {
            rng: ChaCha8Rng::seed_from_u64(cfg.seed),
            makers: (0..cfg.makers).map(|i| Account::new(&format!("mm-{i}"))).collect(),
            takers: (0..cfg.takers).map(|i| Account::new(&format!("taker-{i}"))).collect(),
            maker_pending: BTreeSet::new(),
            taker_pending: BTreeSet::new(),
            funding_pending: BTreeSet::new(),
            last_refresh: 0,
            stale_below: 0,
            prev_mark: 0,
            last_progress: 0,
            funded: false,
            cfg,
        }
    }

    /// Maker depth, inventory and capital scale with the trade rate, so the
    /// ladder is not swept every block at high rates (1.0 at 100 trades/block).
    fn scale(&self) -> f64 {
        (self.cfg.trades_per_block as f64 / 100.0).max(1.0)
    }

    pub fn addresses(&self) -> Vec<Address> {
        self.makers.iter().chain(&self.takers).map(|a| a.addr).collect()
    }

    /// Reacts to one finalized block; returns the transactions to submit.
    pub fn on_block(&mut self, st: &State, out: &BlockOutput) -> Vec<SignedTx> {
        for (h, _, _) in &out.responses {
            if self.maker_pending.remove(h) | self.taker_pending.remove(h) | self.funding_pending.remove(h) {
                self.last_progress = st.height;
            }
        }
        if st.height > self.last_progress + 200 {
            self.forget_pending();
            self.last_progress = st.height;
        }
        let mut txs = self.top_up(st);
        if !self.funded {
            // Trade only once the initial faucet transfers are finalized.
            self.funded = self.funding_pending.is_empty();
            return txs;
        }
        let fair: Vec<f64> = (0..st.metas.len()).map(|i| fair_price(st, i)).collect();

        if self.maker_pending.is_empty() && st.height >= self.last_refresh + self.cfg.refresh_blocks {
            self.last_refresh = st.height;
            let refresh = self.refresh_makers(st, &fair);
            self.maker_pending.extend(refresh.iter().map(SignedTx::hash));
            txs.extend(refresh);
        }
        // Keep at most two blocks of taker flow in flight.
        if self.taker_pending.len() <= self.takers.len() {
            let flow = self.taker_flow(st, &fair);
            self.taker_pending.extend(flow.iter().map(SignedTx::hash));
            txs.extend(flow);
        }
        txs
    }

    /// Drops a transaction the mempool refused, so the simulator does not wait for it.
    pub fn rejected(&mut self, h: &Hash32) {
        self.maker_pending.remove(h);
        self.taker_pending.remove(h);
        self.funding_pending.remove(h);
    }

    fn forget_pending(&mut self) {
        self.maker_pending.clear();
        self.taker_pending.clear();
        self.funding_pending.clear();
    }

    /// Faucet transfers for accounts below their target value.
    fn top_up(&mut self, st: &State) -> Vec<SignedTx> {
        if !self.funding_pending.is_empty() {
            return vec![];
        }
        let mut txs = Vec::new();
        let maker_target = 1_000_000.0 * self.scale();
        for (accounts, target) in [(&mut self.makers, maker_target), (&mut self.takers, 300_000.0)] {
            for acct in accounts.iter_mut() {
                let (av, _) = account_view(st, acct.addr);
                if av < target * 0.5 {
                    let calls = ((target - av) / 100_000.0).ceil().clamp(1.0, 50.0) as usize;
                    for _ in 0..calls {
                        txs.push(acct.sign(Action::Faucet { usd: "100000".into() }, st.time_ms));
                    }
                }
            }
        }
        self.funding_pending.extend(txs.iter().map(SignedTx::hash));
        txs
    }

    fn refresh_makers(&mut self, st: &State, fair: &[f64]) -> Vec<SignedTx> {
        self.stale_below = self.prev_mark;
        self.prev_mark = st.next_oid;
        let mut txs = Vec::new();
        for (m, maker) in self.makers.iter_mut().enumerate() {
            let resting: Vec<CancelWire> = st
                .users
                .get(&maker.addr)
                .map_or(vec![], |u| u.open_orders.iter().filter(|(_, o)| *o < self.stale_below).map(|(a, o)| CancelWire { a: *a, o: *o }).collect());
            if !resting.is_empty() {
                txs.push(maker.sign(Action::Cancel { cancels: resting }, st.time_ms));
            }
            let (_, pos) = account_view(st, maker.addr);
            let mut orders = Vec::new();
            for (a, meta) in st.metas.iter().enumerate() {
                let mut s = style(&meta.name);
                let k = (self.cfg.trades_per_block as f64 / 100.0).max(1.0);
                s.maker_sz = (s.maker_sz.0 * k, s.maker_sz.1 * k);
                s.inventory_limit *= k;
                // Lean against inventory: long makers quote lower, short makers higher.
                let skew_bps = -(pos[a] / s.inventory_limit).clamp(-1.0, 1.0) * 6.0;
                let center = fair[a] * (1.0 + skew_bps / 1e4);
                let half = s.half_spread_bps + m as f64 * 0.4;
                for k in 0..s.levels {
                    let off = (half + k as f64 * s.step_bps) / 1e4;
                    for is_buy in [true, false] {
                        let sz = self.rng.gen_range(s.maker_sz.0..s.maker_sz.1);
                        let px = if is_buy { price(center * (1.0 - off), meta.sz_decimals, false) } else { price(center * (1.0 + off), meta.sz_decimals, true) };
                        orders.push(order(a as u32, is_buy, px, decimal(sz, meta.sz_decimals), Tif::Gtc));
                    }
                }
            }
            txs.push(maker.sign(Action::order(orders), st.time_ms));
        }
        txs
    }

    fn taker_flow(&mut self, st: &State, fair: &[f64]) -> Vec<SignedTx> {
        let n_takers = self.takers.len().max(1);
        let mut per_taker: Vec<Vec<OrderWire>> = vec![Vec::new(); n_takers];
        let positions: Vec<Vec<f64>> = self.takers.iter().map(|t| account_view(st, t.addr).1).collect();
        let styles: Vec<Style> = st.metas.iter().map(|m| style(&m.name)).collect();
        let total_w: f64 = styles.iter().map(|s| s.weight).sum();
        // Shared order-flow imbalance per block makes short trends.
        let flow_bias: f64 = self.rng.gen_range(-0.15..0.15);
        let arb: Vec<f64> = (0..st.metas.len())
            .map(|a| {
                let oracle = st.assets[a].prices.oracle_px.0 as f64 / 1e6;
                -((fair[a] / oracle - 1.0) * 1e4 * ARB_LEAN_PER_BP).clamp(-0.25, 0.25)
            })
            .collect();
        for _ in 0..self.cfg.trades_per_block {
            let t = self.rng.gen_range(0..n_takers);
            let mut pick = self.rng.gen_range(0.0..total_w);
            let a = styles.iter().position(|s| {
                pick -= s.weight;
                pick < 0.0
            }).unwrap_or(0);
            let s = &styles[a];
            let lean = (positions[t][a] / s.inventory_limit).clamp(-1.0, 1.0) * 0.4;
            let is_buy = self.rng.gen_bool((0.5 - lean + flow_bias + arb[a]).clamp(0.05, 0.95));
            let sz = self.rng.gen_range(s.taker_sz.0..s.taker_sz.1);
            let meta = &st.metas[a];
            let limit = if is_buy { price(fair[a] * 1.002, meta.sz_decimals, true) } else { price(fair[a] * 0.998, meta.sz_decimals, false) };
            per_taker[t].push(order(a as u32, is_buy, limit, decimal(sz, meta.sz_decimals), Tif::Ioc));
        }
        self.takers
            .iter_mut()
            .zip(per_taker)
            .filter(|(_, orders)| !orders.is_empty())
            .map(|(acct, orders)| acct.sign(Action::order(orders), st.time_ms))
            .collect()
    }
}

/// Runs the simulator against the node until the block stream closes.
pub async fn run_market_sim(shared: Arc<Shared>, cfg: SimConfig) {
    if cfg.trades_per_block == 0 {
        return;
    }
    let mut sim = MarketSim::new(cfg);
    tracing::info!(trades_per_block = sim.cfg.trades_per_block, makers = sim.makers.len(), takers = sim.takers.len(), "market simulator started");
    let mut blocks = shared.outputs.subscribe();
    loop {
        let out = match blocks.recv().await {
            Ok(out) => out,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        let txs = {
            let st = shared.state.read().expect("state lock");
            sim.on_block(&st, &out)
        };
        for tx in txs {
            let h = tx.hash();
            if !shared.submit(tx) {
                sim.rejected(&h);
            }
        }
    }
}
