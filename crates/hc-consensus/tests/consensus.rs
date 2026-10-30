use hc_consensus::{
    run_deterministic, spawn_tokio, Finalized, LiveConfig, PayloadSource, SimConfig,
};
use std::{sync::Arc, time::Duration};

/// Toy application: payload = "hc|<idx>|<view>|<now_ms>|" ++ parent digest.
struct Toy {
    idx: u32,
}

impl PayloadSource for Toy {
    fn build(&self, view: u64, parent_digest: [u8; 32], now_ms: u64) -> Vec<u8> {
        let mut out = format!("hc|{}|{view}|{now_ms}|", self.idx).into_bytes();
        out.extend_from_slice(&parent_digest);
        out
    }

    fn validate(&self, view: u64, payload: &[u8]) -> bool {
        // Structural check: correct prefix and view.
        let Some(text) = payload.get(..payload.len().saturating_sub(32)) else {
            return false;
        };
        let Ok(text) = std::str::from_utf8(text) else {
            return false;
        };
        let parts: Vec<&str> = text.split('|').collect();
        parts.len() == 5 && parts[0] == "hc" && parts[2] == view.to_string()
    }
}

fn sources(n: usize) -> Vec<Arc<dyn PayloadSource>> {
    (0..n)
        .map(|i| Arc::new(Toy { idx: i as u32 }) as Arc<dyn PayloadSource>)
        .collect()
}

/// Check a single validator's chain is internally consistent.
fn check_chain(chain: &[Finalized]) {
    let mut prev_digest = [0u8; 32];
    let mut prev_view = 0;
    for (i, b) in chain.iter().enumerate() {
        assert_eq!(b.height, i as u64 + 1, "heights are consecutive from 1");
        assert!(b.view > prev_view, "views strictly increase");
        // The application saw the right parent digest when building.
        assert_eq!(
            &b.payload[b.payload.len() - 32..],
            &prev_digest,
            "parent link"
        );
        let text = std::str::from_utf8(&b.payload[..b.payload.len() - 32]).unwrap();
        let parts: Vec<&str> = text.split('|').collect();
        assert_eq!(parts[1], b.proposer.to_string(), "proposer matches builder");
        assert_eq!(parts[2], b.view.to_string());
        prev_digest = b.digest;
        prev_view = b.view;
    }
}

#[test]
fn four_validators_finalize_identical_chains() {
    let cfg = SimConfig {
        seed: 1,
        max_finalized: 25,
        ..SimConfig::default()
    };
    let out = run_deterministic(cfg, sources(4));
    assert_eq!(out.len(), 4);
    for chain in &out {
        assert_eq!(chain.len(), 25);
        check_chain(chain);
        assert_eq!(chain, &out[0], "all validators agree");
    }
    // Leadership rotates: more than one proposer appears.
    let proposers: std::collections::BTreeSet<u32> = out[0].iter().map(|b| b.proposer).collect();
    assert!(proposers.len() > 1);
}

#[test]
fn same_seed_is_byte_identical() {
    let cfg = SimConfig {
        seed: 42,
        max_finalized: 20,
        link_latency_ms: 40,
        link_jitter_ms: 15,
        packet_success_rate: 0.95,
        ..SimConfig::default()
    };
    let a = run_deterministic(cfg.clone(), sources(4));
    let b = run_deterministic(cfg, sources(4));
    assert_eq!(a, b);
    for chain in &a {
        assert_eq!(chain.len(), 20);
        check_chain(chain);
    }
}

#[test]
fn one_validator_offline_still_finalizes() {
    let cfg = SimConfig {
        seed: 3,
        max_finalized: 20,
        offline: vec![2],
        ..SimConfig::default()
    };
    let out = run_deterministic(cfg, sources(4));
    assert!(out[2].is_empty(), "offline validator finalizes nothing");
    for i in [0, 1, 3] {
        assert_eq!(out[i].len(), 20, "validator {i}");
        check_chain(&out[i]);
        assert_eq!(out[i], out[0]);
        // The offline validator never proposes a finalized block.
        assert!(out[i].iter().all(|b| b.proposer != 2));
    }
    // Views led by the offline validator are nullified, so views skip.
    let last = out[0].last().unwrap();
    assert!(last.view > last.height);
}

#[test]
fn lossy_links_still_finalize() {
    let cfg = SimConfig {
        seed: 9,
        max_finalized: 20,
        link_latency_ms: 30,
        link_jitter_ms: 10,
        packet_success_rate: 0.8,
        ..SimConfig::default()
    };
    let out = run_deterministic(cfg, sources(4));
    for chain in &out {
        assert_eq!(chain.len(), 20);
        check_chain(chain);
        assert_eq!(chain, &out[0]);
    }
}

#[test]
fn live_tokio_streams_finalized_blocks() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let cfg = LiveConfig {
            block_interval_ms: 20,
            ..LiveConfig::default()
        };
        let handle = spawn_tokio(cfg, sources(4), tx, async move {
            let _ = stop_rx.await;
        })
        .unwrap();

        let mut got = Vec::new();
        while got.len() < 5 {
            let f = tokio::time::timeout(Duration::from_secs(30), rx.recv())
                .await
                .expect("finalized block within 30s")
                .expect("sink open");
            got.push(f);
        }
        check_chain(&got);

        stop_tx.send(()).unwrap();
        tokio::task::spawn_blocking(move || handle.join())
            .await
            .unwrap()
            .expect("runtime thread exits cleanly");
    });
}
