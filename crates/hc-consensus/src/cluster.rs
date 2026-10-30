//! Runtime-agnostic wiring of an in-process validator cluster: keys, p2p
//! transport, simplex engines and application actors.
//!
//! Two transports are supported:
//! - [`start_simulated`]: Commonware's simulated network (latency, jitter,
//!   loss), which only works on the deterministic runtime.
//! - [`start_lookup`]: Commonware's authenticated (encrypted, ed25519
//!   identified) `lookup` p2p over real localhost TCP, for the tokio runtime.

use crate::{
    actor::{self, Actor, FinalizedSink},
    block::GENESIS,
    PayloadSource,
};
use commonware_consensus::{
    simplex::{elector::RoundRobin, scheme::ed25519, Engine, Floor, ForwardPolicy, SkipPolicy},
    types::{Epoch, ViewDelta},
};
use commonware_cryptography::{
    ed25519::{PrivateKey, PublicKey},
    Sha256, Signer as _,
};
use commonware_p2p::{
    authenticated::lookup, simulated, Address, AddressableManager as _, Blocker, Receiver, Sender,
};
use commonware_parallel::Sequential;
use commonware_runtime::{
    buffer::paged::CacheRef, BufferPooler, Clock, Metrics, Network as RNetwork, Quota, Resolver,
    Spawner, Storage,
};
use commonware_utils::{
    ordered::{Map, Set},
    NZUsize, Probability, NZU16,
};
use rand_core::{CryptoRng, Rng};
use std::{
    collections::BTreeSet,
    net::SocketAddr,
    num::{NonZeroU32, NonZeroUsize},
    sync::Arc,
    time::Duration,
};

/// Namespace for consensus signatures and p2p handshakes (domain separation).
const NAMESPACE: &[u8] = b"hc-consensus";

/// p2p channel ids.
const CH_VOTE: u64 = 0;
const CH_CERT: u64 = 1;
const CH_RESOLVER: u64 = 2;
const CH_BLOCKS: u64 = 3;
/// Application gossip (transactions between validator processes).
const CH_GOSSIP: u64 = 4;
/// Most bytes in one gossip message.
pub(crate) const MAX_GOSSIP_BYTES: usize = 4 * 1024 * 1024;

/// Maximum application message size on the wire (block payload + headroom).
const MAX_MESSAGE_SIZE: u32 = 16 * 1024 * 1024;

/// Per-peer, per-channel message rate. Generous (validators are trusted in this
/// MVP) but finite: authenticated p2p sizes its inbound mailboxes from the
/// burst size, so an unbounded quota would try to allocate gigantic queues.
fn quota() -> Quota {
    Quota::per_second(NonZeroU32::new(10_000).unwrap())
}

/// Deterministically derive validator `i`'s signing key from `seed`.
pub(crate) fn validator_key(seed: u64, i: usize) -> PrivateKey {
    // Mix the seed so that (seed, i) pairs map to distinct key seeds.
    let mixed = seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add((i as u64).wrapping_mul(0xD1B5_4A32_D192_ED03))
        ^ 0x6863_2d63_6f6e_7365; // "hc-conse"
    PrivateKey::from_seed(mixed)
}

/// Inputs shared by every transport.
pub(crate) struct Common {
    pub seed: u64,
    pub sources: Vec<Arc<dyn PayloadSource>>,
    /// `sinks[i]` receives validator `i`'s finalized blocks in order.
    pub sinks: Vec<FinalizedSink>,
    /// Delay before a leader builds its payload.
    pub block_interval: Duration,
    /// Expected one-way network delay (used to scale timeouts).
    pub expected_delay: Duration,
    /// Encoded finalization certificate to start from instead of genesis.
    pub floor: Option<Vec<u8>>,
}

struct Identities {
    private: Vec<PrivateKey>,
    keys: Vec<PublicKey>,
    participants: Set<PublicKey>,
}

fn identities(seed: u64, n: usize) -> Identities {
    // Validator index `i` <-> `keys[i]`. Simplex orders participants by public
    // key internally; the actor maps leaders back to our indices.
    let private: Vec<PrivateKey> = (0..n).map(|i| validator_key(seed, i)).collect();
    let keys: Vec<PublicKey> = private.iter().map(|k| k.public_key()).collect();
    let participants: Set<PublicKey> = keys.clone().try_into().expect("unique validator keys");
    Identities {
        private,
        keys,
        participants,
    }
}

type Channel<S, R> = (S, R);

/// Start one validator (application actor + simplex engine) on `vctx`.
#[allow(clippy::too_many_arguments)]
fn start_validator<E, S, R, B>(
    vctx: &E,
    i: usize,
    ids: &Identities,
    common: &Common,
    blocker: B,
    vote: Channel<S, R>,
    cert: Channel<S, R>,
    resolver: Channel<S, R>,
    blocks: Channel<S, R>,
) where
    E: BufferPooler + Clock + CryptoRng + Spawner + Storage + Metrics,
    S: Sender<PublicKey = PublicKey>,
    R: Receiver<PublicKey = PublicKey>,
    B: Blocker<PublicKey = PublicKey>,
{
    let scheme =
        ed25519::Scheme::signer(NAMESPACE, ids.participants.clone(), ids.private[i].clone())
            .expect("signer is a participant");
    // Start from a finalized block (rejoining a running chain) or from genesis.
    let floor = common.floor.as_ref().map(|bytes| {
        use commonware_codec::Decode as _;
        use commonware_cryptography::certificate::Verifier as _;
        commonware_consensus::simplex::types::Finalization::<ed25519::Scheme, commonware_cryptography::sha256::Digest>::decode_cfg(
            bytes.as_slice(),
            &scheme.certificate_codec_config(),
        )
        .expect("floor certificate decodes")
    });
    let tip = floor.as_ref().map_or((0, GENESIS), |f| (commonware_consensus::Viewable::view(f).get(), f.proposal.payload));
    let (app, mailbox) = Actor::new(
        vctx.child("app"),
        actor::Config {
            index: i as u32,
            keys: ids.keys.clone(),
            source: common.sources[i].clone(),
            sink: common.sinks[i].clone(),
            block_interval: common.block_interval,
            tip,
        },
    );
    app.start(blocks);

    // Timeouts scale with link delay so the cluster stays live on slow links.
    let rtt = common.expected_delay * 2;
    let leader_timeout = (common.block_interval + rtt * 3).max(Duration::from_secs(1));
    let certification_timeout = leader_timeout + (rtt * 3).max(Duration::from_secs(1));

    let cfg = commonware_consensus::simplex::Config {
        scheme,
        elector: RoundRobin::<Sha256>::default(),
        blocker,
        automaton: mailbox.clone(),
        relay: mailbox.clone(),
        reporter: mailbox,
        strategy: Sequential,
        partition: format!("hc-consensus-v{i}"),
        mailbox_size: NZUsize!(1024),
        epoch: Epoch::zero(),
        floor: floor.map_or(Floor::Genesis(GENESIS), Floor::Finalized),
        replay_buffer: NZUsize!(1024 * 1024),
        write_buffer: NZUsize!(1024 * 1024),
        page_cache: CacheRef::from_pooler(vctx, NZU16!(1024), NZUsize!(10)),
        leader_timeout,
        certification_timeout,
        timeout_retry: certification_timeout * 2,
        view_retention: ViewDelta::new(10),
        skip: SkipPolicy::Disabled,
        fetch_timeout: Duration::from_secs(1),
        // Forward full blocks to peers that appear not to have voted: a cheap
        // liveness aid on lossy links (complements the actor's own fetching).
        forward: ForwardPolicy::SilentVoters,
        track_historical_votes: false,
    };
    Engine::new(vctx.child("engine"), cfg).start(vote, cert, resolver);
}

/// Parameters of the simulated network.
pub(crate) struct SimLinks {
    pub latency: Duration,
    pub jitter: Duration,
    pub success_rate: f64,
    pub offline: BTreeSet<usize>,
}

/// Start all validators over Commonware's simulated network.
pub(crate) async fn start_simulated<E>(context: E, common: Common, links: SimLinks)
where
    E: BufferPooler + Clock + CryptoRng + Rng + Spawner + Storage + Metrics + RNetwork,
{
    let n = common.sources.len();
    assert!(n > 0, "need at least one validator");
    assert_eq!(common.sinks.len(), n);
    let success_rate =
        Probability::try_from(links.success_rate).expect("packet_success_rate must be in [0, 1]");
    let ids = identities(common.seed, n);

    let (network, oracle) = simulated::Network::new_with_peers(
        context.child("network"),
        simulated::Config {
            max_size: MAX_MESSAGE_SIZE,
            max_peers_per_set: NonZeroUsize::new(n).unwrap(),
            disconnect_on_block: false,
            tracked_peer_sets: NZUsize!(1),
        },
        ids.keys.clone(),
    )
    .await;
    network.start();

    // Offline validators get no links and no engine: they are crashed from genesis.
    let online: Vec<usize> = (0..n).filter(|i| !links.offline.contains(i)).collect();
    for &a in &online {
        for &b in &online {
            if a == b {
                continue;
            }
            let link = simulated::Link {
                latency: links.latency,
                jitter: links.jitter,
                success_rate,
            };
            oracle
                .add_link(ids.keys[a].clone(), ids.keys[b].clone(), link)
                .await
                .expect("add link");
        }
    }

    for &i in &online {
        let vctx = context.child("validator").with_attribute("validator", i);
        let control = oracle.control(ids.keys[i].clone());
        let vote = control.register(CH_VOTE, quota()).await.expect("register");
        let cert = control.register(CH_CERT, quota()).await.expect("register");
        let resolver = control
            .register(CH_RESOLVER, quota())
            .await
            .expect("register");
        let blocks = control
            .register(CH_BLOCKS, quota())
            .await
            .expect("register");
        start_validator(
            &vctx, i, &ids, &common, control, vote, cert, resolver, blocks,
        );
    }
}

/// Start all validators over authenticated `lookup` p2p, validator `i`
/// listening on `addrs[i]`.
pub(crate) fn start_lookup<E>(context: E, common: Common, addrs: Vec<SocketAddr>)
where
    E: BufferPooler + Clock + CryptoRng + Spawner + Storage + Metrics + RNetwork + Resolver,
{
    let n = common.sources.len();
    assert!(n > 0, "need at least one validator");
    assert_eq!(addrs.len(), n);
    assert_eq!(common.sinks.len(), n);
    let ids = identities(common.seed, n);
    for i in 0..n {
        start_lookup_validator(&context, &common, &ids, &addrs, i, None);
    }
}

/// Start only validator `index` (one validator per process), connecting to
/// its peers at `addrs`, with an application gossip channel.
pub(crate) fn start_lookup_one<E>(context: E, common: Common, addrs: Vec<SocketAddr>, index: usize, gossip: crate::Gossip)
where
    E: BufferPooler + Clock + CryptoRng + Spawner + Storage + Metrics + RNetwork + Resolver,
{
    let ids = identities(common.seed, addrs.len());
    start_lookup_validator(&context, &common, &ids, &addrs, index, Some(gossip));
}

fn start_lookup_validator<E>(context: &E, common: &Common, ids: &Identities, addrs: &[SocketAddr], i: usize, gossip: Option<crate::Gossip>)
where
    E: BufferPooler + Clock + CryptoRng + Spawner + Storage + Metrics + RNetwork + Resolver,
{
    let n = addrs.len();
    let peers: Vec<(PublicKey, Address)> = ids.keys.iter().cloned().zip(addrs.iter().map(|a| Address::Symmetric(*a))).collect();
    let vctx = context.child("validator").with_attribute("validator", i);
    let cfg = lookup::Config::local(ids.private[i].clone(), NAMESPACE, addrs[i], NonZeroUsize::new(n).unwrap(), MAX_MESSAGE_SIZE);
    let (mut network, mut oracle) = lookup::Network::new(vctx.child("network"), cfg);
    let _ = oracle.track(0, Map::try_from(peers).expect("unique peers"));
    let vote = network.register(CH_VOTE, quota());
    let cert = network.register(CH_CERT, quota());
    let resolver = network.register(CH_RESOLVER, quota());
    let blocks = network.register(CH_BLOCKS, quota());
    let gossip_channel = gossip.map(|g| (network.register(CH_GOSSIP, quota()), g));
    network.start();
    if let Some((channel, g)) = gossip_channel {
        start_gossip(vctx.child("gossip"), channel, g);
    }
    start_validator(&vctx, i, ids, common, oracle, vote, cert, resolver, blocks);
}

/// Relays application messages: local ones to every peer, peers' ones to
/// the application. Peers are authenticated by the p2p layer.
fn start_gossip<E, S, R>(context: E, (mut tx, mut rx): Channel<S, R>, gossip: crate::Gossip)
where
    E: Spawner + Clock + Metrics,
    S: Sender<PublicKey = PublicKey>,
    R: Receiver<PublicKey = PublicKey>,
{
    let crate::Gossip { mut outbound, inbound } = gossip;
    context.spawn(move |ctx| async move {
        commonware_macros::select_loop! {
            ctx,
            on_stopped => {},
            Some(msg) = outbound.recv() else break => {
                if msg.len() <= MAX_GOSSIP_BYTES {
                    tx.send(commonware_p2p::Recipients::All, bytes::Bytes::from(msg), false);
                }
            },
            Ok((_peer, buf)) = rx.recv() else break => {
                inbound(buf.as_ref().to_vec());
            },
        }
    });
}
