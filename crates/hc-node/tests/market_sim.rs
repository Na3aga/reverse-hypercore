//! The background market simulator against the engine directly (one block of
//! latency per transaction): it reaches its trade rate, USDC is conserved to
//! the µUSDC through trading, fees and funding, and the run is deterministic.

use hc_core::events::Event;
use hc_core::{SignedTx, State};
use hc_node::feeder::{oracle_tx, PriceWalk};
use hc_node::sim::{MarketSim, SimConfig};
use hc_node::views::supply;
use hc_node::{devnet_genesis, validator_key, N_VALIDATORS};

const T0: u64 = 1_760_000_000_000;
const BLOCK_MS: u64 = 100;

struct Run {
    final_hash: hc_core::types::Hash32,
    trades: usize,
    trading_blocks: usize,
    funding_events: usize,
}

fn run(blocks: u64) -> Run {
    let mut g = devnet_genesis(T0);
    // Fast funding so a short run settles several periods.
    g.params.funding_interval_ms = 4_000;
    g.params.premium_sample_ms = 500;
    let mut st = State::genesis(&g);
    let mut sim = MarketSim::new(SimConfig::default());
    let mut walk = PriceWalk::new(42, &g.oracle_pxs, 15);
    let mut out = st.apply_block(1, T0 + BLOCK_MS, &[]);
    let (mut trades, mut trading_blocks, mut funding_events) = (0, 0, 0);
    for h in 2..=blocks {
        let t = T0 + h * BLOCK_MS;
        let mut txs: Vec<SignedTx> = sim.on_block(&st, &out);
        if h % 20 == 0 {
            walk.step();
            for v in 0..N_VALIDATORS {
                txs.push(oracle_tx(&validator_key(v), &walk.validator_prices(v), t));
            }
        }
        out = st.apply_block(h, t, &txs);
        let n = out.events.iter().filter(|e| matches!(e, Event::Fill(f) if f.crossed)).count();
        if n > 0 {
            trades += n;
            trading_blocks += 1;
        }
        funding_events += out.events.iter().filter(|e| matches!(e, Event::Funding { .. })).count();
        if h % 50 == 0 {
            let s = supply(&st);
            assert_eq!(s["discrepancy"], "0", "USDC not conserved at height {h}: {s}");
        }
    }
    Run { final_hash: st.hash(), trades, trading_blocks, funding_events }
}

#[test]
fn simulator_trades_conserve_usdc_and_replay() {
    let a = run(400);
    let per_block = a.trades as f64 / a.trading_blocks as f64;
    assert!(a.trading_blocks > 300, "trading started late: {} blocks", a.trading_blocks);
    assert!((60.0..=160.0).contains(&per_block), "trades per block {per_block}");
    assert!(a.funding_events > 0, "no funding settled");
    let b = run(400);
    assert_eq!(a.final_hash, b.final_hash, "simulator run is not deterministic");
}
